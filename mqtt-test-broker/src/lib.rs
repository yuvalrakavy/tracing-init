//! A fake MQTT v5 broker for the fleet's bridge tests (Store no-hang §14.5).
//!
//! The bridges' hangs are waits on rumqttc's bounded request channel, which drains only while the
//! event loop is polled. A healthy broker never fills that channel, so a test against one proves
//! nothing about them. This broker can **withhold its acknowledgements**: its CONNACK advertises a
//! small `receive_maximum`, so the client stops at that many in-flight QoS 1 publishes; while the
//! acks are held, rumqttc stops taking requests from its channel, the channel fills, and the next
//! `publish(..).await` waits — the saturation a hang needs. [`FakeBroker::release_acks`] lets it
//! drain again, and a client whose event loop is still being polled then completes.
//!
//! Enough of MQTT for that and no more: one client at a time (a reconnect replaces the
//! connection), QoS 0 to the client, QoS 1 and 2 from the client acknowledged (or held), retained
//! flags recorded but not replayed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
use rumqttc::v5::mqttbytes::v5::{
    ConnAck, ConnAckProperties, ConnectReturnCode, Packet, PingResp, PubAck, PubComp, PubRec, Publish, SubAck, SubscribeReasonCode,
    UnsubAck, UnsubAckReason,
};
pub use rumqttc::v5::mqttbytes::QoS;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};

/// A publish the broker received from the client.
#[derive(Debug, Clone)]
pub struct Received {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
}

/// The broker. Dropping it stops it.
pub struct FakeBroker {
    shared: Arc<Shared>,
    port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for FakeBroker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Shared {
    receive_max: u16,
    received: Mutex<Vec<Received>>,
    subscriptions: Mutex<Vec<String>>,
    connections: AtomicUsize,
    hold_acks: AtomicBool,
    /// Packet ids of the PUBACKs held on the current connection.
    held: Mutex<Vec<u16>>,
    /// The current connection's writer.
    to_client: Mutex<Option<mpsc::UnboundedSender<Packet>>>,
    /// Any change a test may wait for: a publish received, a subscription, a connection.
    changed: Notify,
}

impl FakeBroker {
    /// A broker on `127.0.0.1` with an ephemeral port, advertising `receive_maximum = 10`.
    pub async fn start() -> FakeBroker {
        FakeBroker::start_with_receive_max(10).await
    }

    /// A broker advertising `receive_max` in-flight QoS 1/2 publishes per client.
    pub async fn start_with_receive_max(receive_max: u16) -> FakeBroker {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the fake broker");
        let port = listener.local_addr().expect("the fake broker's address").port();
        let shared = Arc::new(Shared {
            receive_max,
            received: Mutex::new(Vec::new()),
            subscriptions: Mutex::new(Vec::new()),
            connections: AtomicUsize::new(0),
            hold_acks: AtomicBool::new(false),
            held: Mutex::new(Vec::new()),
            to_client: Mutex::new(None),
            changed: Notify::new(),
        });
        let task = tokio::spawn(serve(listener, shared.clone()));
        FakeBroker { shared, port, task }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// `127.0.0.1:<port>`.
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Send a QoS 0 publish to the connected client. `false` when no client is connected.
    pub fn send(&self, topic: &str, payload: impl Into<Bytes>) -> bool {
        let publish = Publish::new(topic, QoS::AtMostOnce, payload.into(), None);
        match &*self.shared.to_client.lock().unwrap() {
            Some(tx) => tx.send(Packet::Publish(publish)).is_ok(),
            None => false,
        }
    }

    /// Stop acknowledging the client's QoS 1 publishes (see the module documentation).
    pub fn hold_acks(&self) {
        self.shared.hold_acks.store(true, Ordering::SeqCst);
    }

    /// Acknowledge everything held, and everything from now on.
    pub fn release_acks(&self) {
        self.shared.hold_acks.store(false, Ordering::SeqCst);
        let held = std::mem::take(&mut *self.shared.held.lock().unwrap());
        if let Some(tx) = &*self.shared.to_client.lock().unwrap() {
            for pkid in held {
                let _ = tx.send(Packet::PubAck(PubAck::new(pkid, None)));
            }
        }
    }

    /// How many PUBACKs are being held right now.
    pub fn held_acks(&self) -> usize {
        self.shared.held.lock().unwrap().len()
    }

    /// Every publish received so far, in order.
    pub fn received(&self) -> Vec<Received> {
        self.shared.received.lock().unwrap().clone()
    }

    /// The publishes received on `topic`, in order.
    pub fn received_on(&self, topic: &str) -> Vec<Received> {
        self.received().into_iter().filter(|r| r.topic == topic).collect()
    }

    /// Every filter subscribed so far (unsubscribes remove theirs).
    pub fn subscriptions(&self) -> Vec<String> {
        self.shared.subscriptions.lock().unwrap().clone()
    }

    /// How many CONNECTs the broker has accepted.
    pub fn connections(&self) -> usize {
        self.shared.connections.load(Ordering::SeqCst)
    }

    /// Wait until `ready(self)` holds, or `within` passes. Returns whether it held.
    pub async fn wait_until(&self, within: Duration, mut ready: impl FnMut(&FakeBroker) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let notified = self.shared.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if ready(self) {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return ready(self);
            }
        }
    }

    /// Wait until the client has subscribed to `filter`.
    pub async fn wait_for_subscription(&self, filter: &str, within: Duration) -> bool {
        self.wait_until(within, |b| b.subscriptions().iter().any(|f| f == filter)).await
    }
}

async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    while let Ok((stream, _)) = listener.accept().await {
        tokio::spawn(connection(stream, shared.clone()));
    }
}

async fn connection(stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Packet>();
    *shared.to_client.lock().unwrap() = Some(tx.clone());
    shared.held.lock().unwrap().clear();
    let writer = tokio::spawn(async move {
        let mut out = BytesMut::new();
        while let Some(packet) = rx.recv().await {
            out.clear();
            if packet.write(&mut out, None).is_err() || wr.write_all(&out).await.is_err() {
                break;
            }
        }
    });
    let mut buf = BytesMut::with_capacity(8 * 1024);
    'read: loop {
        loop {
            match Packet::read(&mut buf, None) {
                Ok(packet) => {
                    let open = handle(packet, &tx, &shared);
                    shared.changed.notify_waiters();
                    if !open {
                        break 'read;
                    }
                }
                Err(rumqttc::v5::mqttbytes::Error::InsufficientBytes(_)) => break,
                Err(_) => break 'read,
            }
        }
        match rd.read_buf(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    writer.abort();
}

/// Answer one packet. `false` when the client disconnected.
fn handle(packet: Packet, tx: &mpsc::UnboundedSender<Packet>, shared: &Shared) -> bool {
    match packet {
        Packet::Connect(..) => {
            shared.connections.fetch_add(1, Ordering::SeqCst);
            let properties = ConnAckProperties {
                session_expiry_interval: None,
                receive_max: Some(shared.receive_max),
                max_qos: None,
                retain_available: None,
                max_packet_size: None,
                assigned_client_identifier: None,
                topic_alias_max: None,
                reason_string: None,
                user_properties: Vec::new(),
                wildcard_subscription_available: None,
                subscription_identifiers_available: None,
                shared_subscription_available: None,
                server_keep_alive: None,
                response_information: None,
                server_reference: None,
                authentication_method: None,
                authentication_data: None,
            };
            let _ = tx.send(Packet::ConnAck(ConnAck {
                session_present: false,
                code: ConnectReturnCode::Success,
                properties: Some(properties),
            }));
        }
        Packet::Subscribe(s) => {
            let codes = s.filters.iter().map(|f| SubscribeReasonCode::Success(f.qos)).collect();
            shared.subscriptions.lock().unwrap().extend(s.filters.iter().map(|f| f.path.clone()));
            let _ = tx.send(Packet::SubAck(SubAck { pkid: s.pkid, return_codes: codes, properties: None }));
        }
        Packet::Unsubscribe(u) => {
            shared.subscriptions.lock().unwrap().retain(|f| !u.filters.contains(f));
            let reasons = u.filters.iter().map(|_| UnsubAckReason::Success).collect();
            let _ = tx.send(Packet::UnsubAck(UnsubAck { pkid: u.pkid, reasons, properties: None }));
        }
        Packet::Publish(p) => {
            shared.received.lock().unwrap().push(Received {
                topic: String::from_utf8_lossy(&p.topic).into_owned(),
                payload: p.payload.clone(),
                qos: p.qos,
                retain: p.retain,
            });
            match p.qos {
                QoS::AtMostOnce => {}
                QoS::AtLeastOnce => {
                    if shared.hold_acks.load(Ordering::SeqCst) {
                        shared.held.lock().unwrap().push(p.pkid);
                    } else {
                        let _ = tx.send(Packet::PubAck(PubAck::new(p.pkid, None)));
                    }
                }
                QoS::ExactlyOnce => {
                    let _ = tx.send(Packet::PubRec(PubRec::new(p.pkid, None)));
                }
            }
        }
        Packet::PubRel(r) => {
            let _ = tx.send(Packet::PubComp(PubComp::new(r.pkid, None)));
        }
        Packet::PingReq(_) => {
            let _ = tx.send(Packet::PingResp(PingResp));
        }
        Packet::Disconnect(_) => return false,
        _ => {}
    }
    true
}

#[cfg(test)]
mod tests;

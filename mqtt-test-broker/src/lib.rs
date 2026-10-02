//! A fake MQTT broker (v5 or v3.1.1) for the fleet's bridge tests (Store no-hang §14.5).
//!
//! The bridges' hangs are waits on rumqttc's bounded request channel, which drains only while the
//! event loop is polled. A healthy broker never fills that channel, so a test against one proves
//! nothing about them. This broker can **withhold its acknowledgements**: the client stops at its
//! in-flight limit of QoS 1 publishes (v5: the `receive_maximum` this broker's CONNACK advertises;
//! v3.1.1: the client's own `MqttOptions::set_inflight`); while the acks are held, rumqttc stops
//! taking requests from its channel, the channel fills, and the next `publish(..).await` waits —
//! the saturation a hang needs. [`FakeBroker::release_acks`] lets it drain again, and a client
//! whose event loop is still being polled then completes.
//!
//! Enough of MQTT for that and no more: one client at a time (a reconnect replaces the
//! connection), QoS 0 to the client, QoS 1 from the client acknowledged or held, QoS 2 from the
//! client always acknowledged (PUBREC, then PUBCOMP for its PUBREL — never held, so a QoS 2
//! publish does not saturate a client), retained flags recorded but not replayed.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Bytes, BytesMut};
pub use rumqttc::v5::mqttbytes::QoS;
use rumqttc::{mqttbytes::v4, v5::mqttbytes::v5};
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

/// The protocol the broker speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    V5,
    /// MQTT 3.1.1, rumqttc's `v4` module.
    V4,
}

/// The broker. Dropping it stops it: the listener and every connection close (their tasks are
/// aborted, and end at the runtime's next turn).
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
    protocol: Protocol,
    receive_max: u16,
    received: Mutex<Vec<Received>>,
    subscriptions: Mutex<Vec<String>>,
    connections: AtomicUsize,
    /// Whether QoS 1 acks are held. Changed and read only under `held`, so an ack's decision and
    /// its push are one step against a release (review finding X-7).
    hold_acks: AtomicBool,
    /// Packet ids of the PUBACKs held on the current connection.
    held: Mutex<Vec<u16>>,
    /// The current connection's writer, taking encoded packets.
    to_client: Mutex<Option<mpsc::UnboundedSender<BytesMut>>>,
    /// Any change a test may wait for: a publish received, a subscription, a connection.
    changed: Notify,
}

impl FakeBroker {
    /// A v5 broker on `127.0.0.1` with an ephemeral port, advertising `receive_maximum = 10`.
    pub async fn start() -> FakeBroker {
        FakeBroker::start_with_receive_max(10).await
    }

    /// A v5 broker advertising `receive_max` in-flight QoS 1/2 publishes per client.
    pub async fn start_with_receive_max(receive_max: u16) -> FakeBroker {
        FakeBroker::start_protocol(Protocol::V5, receive_max).await
    }

    /// A v3.1.1 broker. The in-flight limit is the client's (`MqttOptions::set_inflight`).
    pub async fn start_v4() -> FakeBroker {
        FakeBroker::start_protocol(Protocol::V4, 0).await
    }

    async fn start_protocol(protocol: Protocol, receive_max: u16) -> FakeBroker {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the fake broker");
        let port = listener.local_addr().expect("the fake broker's address").port();
        let shared = Arc::new(Shared {
            protocol,
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
        let payload = payload.into();
        let encoded = match self.shared.protocol {
            Protocol::V5 => encode_v5(v5::Packet::Publish(v5::Publish::new(topic, QoS::AtMostOnce, payload, None))),
            Protocol::V4 => encode_v4(v4::Packet::Publish(v4::Publish::new(topic, v4_qos(QoS::AtMostOnce), payload.to_vec()))),
        };
        match &*self.shared.to_client.lock().unwrap() {
            Some(tx) => tx.send(encoded).is_ok(),
            None => false,
        }
    }

    /// Stop acknowledging the client's QoS 1 publishes (see the module documentation). QoS 2
    /// publishes are still acknowledged.
    pub fn hold_acks(&self) {
        let _held = self.shared.held.lock().unwrap();
        self.shared.hold_acks.store(true, Ordering::SeqCst);
    }

    /// Acknowledge everything held, and everything from now on. The flag turns and the held acks
    /// go out under the `held` lock, so no ack decided meanwhile is left behind, and the released
    /// acks precede any later one.
    pub fn release_acks(&self) {
        let mut held = self.shared.held.lock().unwrap();
        self.shared.hold_acks.store(false, Ordering::SeqCst);
        let held = std::mem::take(&mut *held);
        if let Some(tx) = &*self.shared.to_client.lock().unwrap() {
            for pkid in held {
                let _ = tx.send(puback(self.shared.protocol, pkid));
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

/// Accept connections. Each runs in this task's `JoinSet`, so aborting this task — dropping the
/// broker — drops the set, which aborts every connection (review finding C-19).
async fn serve(listener: TcpListener, shared: Arc<Shared>) {
    let mut connections = tokio::task::JoinSet::new();
    while let Ok((stream, _)) = listener.accept().await {
        while connections.try_join_next().is_some() {}
        connections.spawn(connection(stream, shared.clone()));
    }
}

/// What reading one packet off the buffer came to.
enum Read {
    /// A packet was handled; the connection stays open.
    Handled,
    /// The client disconnected.
    Closed,
    /// The buffer holds no whole packet yet.
    NeedMore,
}

async fn connection(stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<BytesMut>();
    *shared.to_client.lock().unwrap() = Some(tx.clone());
    shared.held.lock().unwrap().clear();
    // Aborted when the client goes. When this task is aborted instead (the broker dropped), the
    // writer ends when the last sender does: `to_client`'s, freed with the broker's `Shared`.
    let writer = tokio::spawn(async move {
        while let Some(packet) = rx.recv().await {
            if wr.write_all(&packet).await.is_err() {
                break;
            }
        }
    });
    let mut buf = BytesMut::with_capacity(8 * 1024);
    'read: loop {
        loop {
            let read = match shared.protocol {
                Protocol::V5 => read_v5(&mut buf, &tx, &shared),
                Protocol::V4 => read_v4(&mut buf, &tx, &shared),
            };
            match read {
                Some(Read::Handled) => shared.changed.notify_waiters(),
                Some(Read::NeedMore) => break,
                Some(Read::Closed) | None => {
                    shared.changed.notify_waiters();
                    break 'read;
                }
            }
        }
        match rd.read_buf(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    writer.abort();
}

fn record(shared: &Shared, topic: String, payload: Bytes, qos: QoS, retain: bool) {
    shared.received.lock().unwrap().push(Received { topic, payload, qos, retain });
}

/// Answer a QoS 1 publish now, or hold its ack — decided and done under the `held` lock, which the
/// hold flag changes under: read first and pushed after, a release in between would take the held
/// acks before this one joined them, and strand it (review finding X-7).
fn ack_or_hold(shared: &Shared, tx: &mpsc::UnboundedSender<BytesMut>, pkid: u16) {
    let mut held = shared.held.lock().unwrap();
    if shared.hold_acks.load(Ordering::SeqCst) {
        held.push(pkid);
    } else {
        let _ = tx.send(puback(shared.protocol, pkid));
    }
}

fn puback(protocol: Protocol, pkid: u16) -> BytesMut {
    match protocol {
        Protocol::V5 => encode_v5(v5::Packet::PubAck(v5::PubAck::new(pkid, None))),
        Protocol::V4 => encode_v4(v4::Packet::PubAck(v4::PubAck::new(pkid))),
    }
}

fn encode_v5(packet: v5::Packet) -> BytesMut {
    let mut out = BytesMut::new();
    packet.write(&mut out, None).expect("encode a v5 packet");
    out
}

fn encode_v4(packet: v4::Packet) -> BytesMut {
    let mut out = BytesMut::new();
    packet.write(&mut out, usize::MAX).expect("encode a v4 packet");
    out
}

fn v4_qos(q: QoS) -> rumqttc::mqttbytes::QoS {
    match q {
        QoS::AtMostOnce => rumqttc::mqttbytes::QoS::AtMostOnce,
        QoS::AtLeastOnce => rumqttc::mqttbytes::QoS::AtLeastOnce,
        QoS::ExactlyOnce => rumqttc::mqttbytes::QoS::ExactlyOnce,
    }
}

fn from_v4_qos(q: rumqttc::mqttbytes::QoS) -> QoS {
    match q {
        rumqttc::mqttbytes::QoS::AtMostOnce => QoS::AtMostOnce,
        rumqttc::mqttbytes::QoS::AtLeastOnce => QoS::AtLeastOnce,
        rumqttc::mqttbytes::QoS::ExactlyOnce => QoS::ExactlyOnce,
    }
}

/// Read and answer one v5 packet; `None` on a malformed stream.
fn read_v5(buf: &mut BytesMut, tx: &mpsc::UnboundedSender<BytesMut>, shared: &Shared) -> Option<Read> {
    let packet = match v5::Packet::read(buf, None) {
        Ok(p) => p,
        Err(rumqttc::v5::mqttbytes::Error::InsufficientBytes(_)) => return Some(Read::NeedMore),
        Err(_) => return None,
    };
    match packet {
        v5::Packet::Connect(..) => {
            shared.connections.fetch_add(1, Ordering::SeqCst);
            let properties = v5::ConnAckProperties {
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
            let ack = v5::ConnAck { session_present: false, code: v5::ConnectReturnCode::Success, properties: Some(properties) };
            let _ = tx.send(encode_v5(v5::Packet::ConnAck(ack)));
        }
        v5::Packet::Subscribe(s) => {
            let codes = s.filters.iter().map(|f| v5::SubscribeReasonCode::Success(f.qos)).collect();
            shared.subscriptions.lock().unwrap().extend(s.filters.iter().map(|f| f.path.clone()));
            let _ = tx.send(encode_v5(v5::Packet::SubAck(v5::SubAck { pkid: s.pkid, return_codes: codes, properties: None })));
        }
        v5::Packet::Unsubscribe(u) => {
            shared.subscriptions.lock().unwrap().retain(|f| !u.filters.contains(f));
            let reasons = u.filters.iter().map(|_| v5::UnsubAckReason::Success).collect();
            let _ = tx.send(encode_v5(v5::Packet::UnsubAck(v5::UnsubAck { pkid: u.pkid, reasons, properties: None })));
        }
        v5::Packet::Publish(p) => {
            record(shared, String::from_utf8_lossy(&p.topic).into_owned(), p.payload.clone(), p.qos, p.retain);
            match p.qos {
                QoS::AtMostOnce => {}
                QoS::AtLeastOnce => ack_or_hold(shared, tx, p.pkid),
                QoS::ExactlyOnce => {
                    let _ = tx.send(encode_v5(v5::Packet::PubRec(v5::PubRec::new(p.pkid, None))));
                }
            }
        }
        v5::Packet::PubRel(r) => {
            let _ = tx.send(encode_v5(v5::Packet::PubComp(v5::PubComp::new(r.pkid, None))));
        }
        v5::Packet::PingReq(_) => {
            let _ = tx.send(encode_v5(v5::Packet::PingResp(v5::PingResp)));
        }
        v5::Packet::Disconnect(_) => return Some(Read::Closed),
        _ => {}
    }
    Some(Read::Handled)
}

/// Read and answer one v3.1.1 packet; `None` on a malformed stream.
fn read_v4(buf: &mut BytesMut, tx: &mpsc::UnboundedSender<BytesMut>, shared: &Shared) -> Option<Read> {
    let packet = match v4::Packet::read(buf, usize::MAX) {
        Ok(p) => p,
        Err(rumqttc::mqttbytes::Error::InsufficientBytes(_)) => return Some(Read::NeedMore),
        Err(_) => return None,
    };
    match packet {
        v4::Packet::Connect(_) => {
            shared.connections.fetch_add(1, Ordering::SeqCst);
            let _ = tx.send(encode_v4(v4::Packet::ConnAck(v4::ConnAck::new(v4::ConnectReturnCode::Success, false))));
        }
        v4::Packet::Subscribe(s) => {
            let codes = s.filters.iter().map(|f| v4::SubscribeReasonCode::Success(f.qos)).collect();
            shared.subscriptions.lock().unwrap().extend(s.filters.iter().map(|f| f.path.clone()));
            let _ = tx.send(encode_v4(v4::Packet::SubAck(v4::SubAck::new(s.pkid, codes))));
        }
        v4::Packet::Unsubscribe(u) => {
            shared.subscriptions.lock().unwrap().retain(|f| !u.topics.contains(f));
            let _ = tx.send(encode_v4(v4::Packet::UnsubAck(v4::UnsubAck::new(u.pkid))));
        }
        v4::Packet::Publish(p) => {
            let qos = from_v4_qos(p.qos);
            record(shared, p.topic.clone(), p.payload.clone(), qos, p.retain);
            match qos {
                QoS::AtMostOnce => {}
                QoS::AtLeastOnce => ack_or_hold(shared, tx, p.pkid),
                QoS::ExactlyOnce => {
                    let _ = tx.send(encode_v4(v4::Packet::PubRec(v4::PubRec::new(p.pkid))));
                }
            }
        }
        v4::Packet::PubRel(r) => {
            let _ = tx.send(encode_v4(v4::Packet::PubComp(v4::PubComp::new(r.pkid))));
        }
        v4::Packet::PingReq => {
            let _ = tx.send(encode_v4(v4::Packet::PingResp));
        }
        v4::Packet::Disconnect => return Some(Read::Closed),
        _ => {}
    }
    Some(Read::Handled)
}

#[cfg(test)]
mod tests;

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
//! publish does not saturate a client).
//!
//! For the Bridge Runtime's conformance suite (tracing-init `mqtt-bridge-conformance`) it also
//! plays the rest of a broker a bridge relies on, still for one client:
//! - **v5 properties both ways:** [`FakeBroker::send_with`] sends a publish with a message expiry,
//!   user properties, a response topic and correlation data ([`SendOptions`]); a publish received
//!   keeps its properties ([`Received::properties`]).
//! - **Retained messages:** a retained publish from the client is kept (an empty one clears its
//!   topic), and so is one the test leaves with [`FakeBroker::retain`] — as another client would;
//!   each is replayed, flagged retained and with its remaining expiry, on every subscription that
//!   matches it, after the SUBACK.
//! - **Routing:** a publish from the client goes back to it on its current connection's matching
//!   subscriptions, unflagged (retain-as-published off), as a broker forwards it.
//! - **Faults:** [`FakeBroker::drop_connection`] cuts the connection, as a network fault does;
//!   [`FakeBroker::restart_without_persistence`] cuts it and forgets every retained message and
//!   subscription, as a broker restarted without persistence does.
//! - **Order:** [`FakeBroker::events`] lists connections, subscriptions and publishes in the order
//!   the broker saw them.
//! - **A cap:** [`FakeBroker::cap_connections`] stops the listener past a number of connections, so
//!   a client that reconnects in a loop is refused by the operating system rather than spinning
//!   through the host's ephemeral ports.
//! - **A wedged connection:** [`FakeBroker::stop_reading`] stops reading the current connection —
//!   no packet of it is handled any more, PINGREQ included — as a broker that stopped reading
//!   does, so a client that goes on writing fills the sockets' buffers and blocks in its flush.
//!   A new connection reads again.
//! - **A broker that is down:** [`FakeBroker::refuse_connections`] closes every new connection
//!   at once, unanswered, until it is turned off.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
pub use rumqttc::v5::mqttbytes::QoS;
use rumqttc::{mqttbytes::v4, v5::mqttbytes::v5};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch, Notify};

pub use rumqttc::v5::mqttbytes::v5::PublishProperties;

/// A publish the broker received from the client.
#[derive(Debug, Clone)]
pub struct Received {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    /// Its v5 properties, if it had any (always `None` from a v3.1.1 client).
    pub properties: Option<PublishProperties>,
}

impl Received {
    /// The value of the user property `name`, if the publish carried one.
    pub fn user_property(&self, name: &str) -> Option<&str> {
        self.properties.as_ref()?.user_properties.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    /// The publish's correlation data, if it carried any.
    pub fn correlation_data(&self) -> Option<&Bytes> {
        self.properties.as_ref()?.correlation_data.as_ref()
    }
}

/// How the broker sends a publish of its own ([`FakeBroker::send_with`]) or keeps a retained one
/// ([`FakeBroker::retain`]). The v5 properties are dropped on a v3.1.1 connection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SendOptions {
    /// Flag the publish retained, as a broker does when it replays a retained message.
    pub retain: bool,
    /// The message expiry interval, in seconds.
    pub message_expiry: Option<u32>,
    pub user_properties: Vec<(String, String)>,
    pub response_topic: Option<String>,
    pub correlation_data: Option<Bytes>,
}

impl SendOptions {
    pub fn retained(mut self) -> SendOptions {
        self.retain = true;
        self
    }

    pub fn expiry(mut self, seconds: u32) -> SendOptions {
        self.message_expiry = Some(seconds);
        self
    }

    pub fn user_property(mut self, name: &str, value: &str) -> SendOptions {
        self.user_properties.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn response_topic(mut self, topic: &str) -> SendOptions {
        self.response_topic = Some(topic.to_owned());
        self
    }

    pub fn correlation_data(mut self, data: impl Into<Bytes>) -> SendOptions {
        self.correlation_data = Some(data.into());
        self
    }

    fn properties(&self, expiry: Option<u32>) -> Option<PublishProperties> {
        if expiry.is_none() && self.user_properties.is_empty() && self.response_topic.is_none() && self.correlation_data.is_none() {
            return None;
        }
        Some(PublishProperties {
            message_expiry_interval: expiry,
            user_properties: self.user_properties.clone(),
            response_topic: self.response_topic.clone(),
            correlation_data: self.correlation_data.clone(),
            ..PublishProperties::default()
        })
    }
}

/// Something the broker saw, in order ([`FakeBroker::events`]).
#[derive(Debug, Clone)]
pub enum BrokerEvent {
    /// A CONNECT was accepted.
    Connected,
    /// The client subscribed to this filter.
    Subscribed(String),
    Unsubscribed(String),
    /// A publish from the client.
    Published(Received),
    /// The client sent DISCONNECT.
    Disconnected,
}

/// A retained message: what the client or the test left on a topic.
#[derive(Debug, Clone)]
struct Retained {
    payload: Bytes,
    options: SendOptions,
    stored_at: Instant,
}

impl Retained {
    /// Its expiry as a replay now gives it: what is left of it, or `None` when it has expired.
    fn remaining(&self) -> Option<Option<u32>> {
        match self.options.message_expiry {
            None => Some(None),
            Some(expiry) => {
                let elapsed = self.stored_at.elapsed().as_secs();
                (elapsed < expiry as u64).then(|| Some(expiry - elapsed as u32))
            }
        }
    }
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
    /// The retained messages, by topic.
    retained: Mutex<BTreeMap<String, Retained>>,
    /// The current connection's subscriptions (`subscriptions` keeps every one ever made).
    active: Mutex<Vec<String>>,
    events: Mutex<Vec<BrokerEvent>>,
    /// Bumped to close the current connection (`drop_connection`).
    kick: watch::Sender<u64>,
    /// The listener stops past this many accepted connections.
    cap: AtomicUsize,
    accepted: AtomicUsize,
    /// The connection `stop_reading` wedged, by its number (`accepted` when it was taken).
    wedged: watch::Sender<Option<usize>>,
    /// The current connection's number.
    current: AtomicUsize,
    /// Close every new connection at once (`refuse_connections`).
    refusing: AtomicBool,
    /// Arrivals at the `held` lock (see `lock_held`).
    #[cfg(test)]
    held_lock_reached: AtomicUsize,
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
            retained: Mutex::new(BTreeMap::new()),
            active: Mutex::new(Vec::new()),
            events: Mutex::new(Vec::new()),
            kick: watch::Sender::new(0),
            cap: AtomicUsize::new(usize::MAX),
            accepted: AtomicUsize::new(0),
            wedged: watch::Sender::new(None),
            current: AtomicUsize::new(0),
            refusing: AtomicBool::new(false),
            #[cfg(test)]
            held_lock_reached: AtomicUsize::new(0),
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
        self.send_with(topic, payload, &SendOptions::default())
    }

    /// Send a QoS 0 publish with `options` (its retain flag and v5 properties) to the connected
    /// client. `false` when no client is connected.
    pub fn send_with(&self, topic: &str, payload: impl Into<Bytes>, options: &SendOptions) -> bool {
        let encoded =
            encode_publish(self.shared.protocol, topic, payload.into(), options.retain, options.properties(options.message_expiry));
        match &*self.shared.to_client.lock().unwrap() {
            Some(tx) => tx.send(encoded).is_ok(),
            None => false,
        }
    }

    /// Leave a retained message on `topic`, as another client publishing it retained would: it is
    /// replayed on every later subscription that matches it, with what is left of its expiry. An
    /// empty payload clears the topic. Nothing is sent to a subscription already made.
    pub fn retain(&self, topic: &str, payload: impl Into<Bytes>, options: &SendOptions) {
        store_retained(&self.shared, topic, payload.into(), options.clone());
    }

    /// The payload retained on `topic`, if any (an expired one counts as none).
    pub fn retained_on(&self, topic: &str) -> Option<Bytes> {
        let retained = self.shared.retained.lock().unwrap();
        retained.get(topic).filter(|r| r.remaining().is_some()).map(|r| r.payload.clone())
    }

    /// Every topic with a retained message, and its payload.
    pub fn retained(&self) -> Vec<(String, Bytes)> {
        let retained = self.shared.retained.lock().unwrap();
        retained.iter().filter(|(_, r)| r.remaining().is_some()).map(|(t, r)| (t.clone(), r.payload.clone())).collect()
    }

    /// Everything the broker saw, in order.
    pub fn events(&self) -> Vec<BrokerEvent> {
        self.shared.events.lock().unwrap().clone()
    }

    /// Close the current connection, as a network fault would. The client may reconnect.
    pub fn drop_connection(&self) {
        self.shared.kick.send_modify(|k| *k += 1);
    }

    /// Close the current connection and forget every retained message and subscription, as a
    /// broker restarted without persistence would. The client may reconnect.
    pub fn restart_without_persistence(&self) {
        self.shared.retained.lock().unwrap().clear();
        self.shared.active.lock().unwrap().clear();
        self.drop_connection();
    }

    /// Accept at most `cap` connections in all, then stop listening (the connection open then
    /// stays open): a client that reconnects in a loop is refused by the operating system from
    /// then on, instead of spinning through the host's ephemeral ports.
    pub fn cap_connections(&self, cap: usize) {
        self.shared.cap.store(cap, Ordering::SeqCst);
    }

    /// Stop reading the current connection, as a broker that stopped reading does: nothing more
    /// of it is handled (no PINGRESP, no ack), and it stays open, so a client that goes on writing
    /// fills the sockets' buffers and then waits in its flush. A new connection is read again.
    pub fn stop_reading(&self) {
        self.shared.wedged.send_replace(Some(self.shared.current.load(Ordering::SeqCst)));
    }

    /// Close every new connection at once, unanswered (a broker that is down), or (`false`)
    /// serve them again. Refused connections count towards the cap.
    pub fn refuse_connections(&self, refusing: bool) {
        self.shared.refusing.store(refusing, Ordering::SeqCst);
    }

    /// How many TCP connections the broker has accepted.
    pub fn accepted(&self) -> usize {
        self.shared.accepted.load(Ordering::SeqCst)
    }

    /// Stop acknowledging the client's QoS 1 publishes (see the module documentation). QoS 2
    /// publishes are still acknowledged.
    pub fn hold_acks(&self) {
        let _held = lock_held(&self.shared);
        self.shared.hold_acks.store(true, Ordering::SeqCst);
    }

    /// Acknowledge everything held, and everything from now on. The flag turns and the held acks
    /// go out under the `held` lock, so no ack decided meanwhile is left behind, and the released
    /// acks precede any later one.
    pub fn release_acks(&self) {
        let mut held = lock_held(&self.shared);
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
        lock_held(&self.shared).len()
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
        let accepted = shared.accepted.fetch_add(1, Ordering::SeqCst) + 1;
        if accepted > shared.cap.load(Ordering::SeqCst) {
            drop(stream);
            break;
        }
        if shared.refusing.load(Ordering::SeqCst) {
            drop(stream);
            shared.changed.notify_waiters();
            continue;
        }
        connections.spawn(connection(stream, shared.clone(), accepted));
    }
    // Past the cap: no more connections, and the open ones go on until they end or the broker is
    // dropped (which aborts this task, and with it the set).
    drop(listener);
    while connections.join_next().await.is_some() {}
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

/// A task aborted when this is dropped. A plain `JoinHandle` detaches its task instead.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn connection(stream: TcpStream, shared: Arc<Shared>, number: usize) {
    let _ = stream.set_nodelay(true);
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<BytesMut>();
    *shared.to_client.lock().unwrap() = Some(tx.clone());
    lock_held(&shared).clear();
    // A new connection starts without a session: no subscription carries over.
    shared.active.lock().unwrap().clear();
    let mut kick = shared.kick.subscribe();
    kick.mark_unchanged();
    shared.current.store(number, Ordering::SeqCst);
    let mut wedged = shared.wedged.subscribe();
    // The writer goes with this task: when the client goes, and when the task is aborted (the
    // broker dropped) — also while the writer is blocked in `write_all` on a client that stopped
    // reading, which a detached writer survived, holding the client's socket open (review
    // finding T4).
    let _writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(packet) = rx.recv().await {
            if wr.write_all(&packet).await.is_err() {
                break;
            }
        }
    }));
    let mut buf = BytesMut::with_capacity(8 * 1024);
    'read: loop {
        while *wedged.borrow_and_update() == Some(number) {
            // Wedged (`stop_reading`): nothing more of this connection is read or handled.
            tokio::select! {
                _ = wedged.changed() => {}
                _ = kick.changed() => break 'read,
            }
        }
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
        tokio::select! {
            read = rd.read_buf(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            },
            _ = kick.changed() => break,
        }
    }
    shared.changed.notify_waiters();
}

fn record(shared: &Shared, received: Received) {
    shared.events.lock().unwrap().push(BrokerEvent::Published(received.clone()));
    shared.received.lock().unwrap().push(received);
}

fn event(shared: &Shared, event: BrokerEvent) {
    shared.events.lock().unwrap().push(event);
}

/// Keep `payload` retained on `topic`, or clear the topic for an empty one.
fn store_retained(shared: &Shared, topic: &str, payload: Bytes, options: SendOptions) {
    let mut retained = shared.retained.lock().unwrap();
    if payload.is_empty() {
        retained.remove(topic);
    } else {
        retained.insert(topic.to_owned(), Retained { payload, options, stored_at: Instant::now() });
    }
}

/// A publish from the client: kept if retained, then forwarded to the client's own subscriptions
/// that match it, once, unflagged.
fn route(
    shared: &Shared,
    tx: &mpsc::UnboundedSender<BytesMut>,
    topic: &str,
    payload: &Bytes,
    retain: bool,
    properties: Option<PublishProperties>,
) {
    if retain {
        let options = SendOptions {
            retain: true,
            message_expiry: properties.as_ref().and_then(|p| p.message_expiry_interval),
            user_properties: properties.as_ref().map(|p| p.user_properties.clone()).unwrap_or_default(),
            response_topic: properties.as_ref().and_then(|p| p.response_topic.clone()),
            correlation_data: properties.as_ref().and_then(|p| p.correlation_data.clone()),
        };
        store_retained(shared, topic, payload.clone(), options);
    }
    let subscribed = shared.active.lock().unwrap().iter().any(|filter| rumqttc::v5::mqttbytes::matches(topic, filter));
    if subscribed {
        let _ = tx.send(encode_publish(shared.protocol, topic, payload.clone(), false, properties));
    }
}

/// After a SUBACK: the retained messages matching `filter`, flagged retained, each with what is
/// left of its expiry (an expired one is dropped).
fn replay(shared: &Shared, tx: &mpsc::UnboundedSender<BytesMut>, filter: &str) {
    let mut retained = shared.retained.lock().unwrap();
    retained.retain(|_, r| r.remaining().is_some());
    for (topic, r) in retained.iter() {
        if rumqttc::v5::mqttbytes::matches(topic, filter) {
            let expiry = r.remaining().flatten();
            let _ = tx.send(encode_publish(shared.protocol, topic, r.payload.clone(), true, r.options.properties(expiry)));
        }
    }
}

/// A QoS 0 publish to the client.
fn encode_publish(protocol: Protocol, topic: &str, payload: Bytes, retain: bool, properties: Option<PublishProperties>) -> BytesMut {
    match protocol {
        Protocol::V5 => {
            let mut publish = v5::Publish::new(topic, QoS::AtMostOnce, payload, properties);
            publish.retain = retain;
            encode_v5(v5::Packet::Publish(publish))
        }
        Protocol::V4 => {
            let mut publish = v4::Publish::new(topic, v4_qos(QoS::AtMostOnce), payload.to_vec());
            publish.retain = retain;
            encode_v4(v4::Packet::Publish(publish))
        }
    }
}

/// The `held` lock. Under test, each arrival at it is counted first, so a test can tell a thread
/// has reached it — and decided whatever it decides before it — instead of sleeping on that
/// (review finding T1).
fn lock_held(shared: &Shared) -> std::sync::MutexGuard<'_, Vec<u16>> {
    #[cfg(test)]
    shared.held_lock_reached.fetch_add(1, Ordering::SeqCst);
    shared.held.lock().unwrap()
}

/// Answer a QoS 1 publish now, or hold its ack — decided and done under the `held` lock, which the
/// hold flag changes under: read first and pushed after, a release in between would take the held
/// acks before this one joined them, and strand it (review finding X-7).
fn ack_or_hold(shared: &Shared, tx: &mpsc::UnboundedSender<BytesMut>, pkid: u16) {
    let mut held = lock_held(shared);
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
    // A DISCONNECT with reason 0 and no properties has a remaining length of 0 (MQTT v5 §3.14.2.1),
    // as rumqttc's own client writes it — and rumqttc 0.25's reader refuses it (`PayloadRequired`).
    if buf.starts_with(&[0xE0, 0x00]) {
        let _ = buf.split_to(2);
        event(shared, BrokerEvent::Disconnected);
        return Some(Read::Closed);
    }
    let packet = match v5::Packet::read(buf, None) {
        Ok(p) => p,
        Err(rumqttc::v5::mqttbytes::Error::InsufficientBytes(_)) => return Some(Read::NeedMore),
        Err(_) => return None,
    };
    match packet {
        v5::Packet::Connect(..) => {
            shared.connections.fetch_add(1, Ordering::SeqCst);
            event(shared, BrokerEvent::Connected);
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
            shared.active.lock().unwrap().extend(s.filters.iter().map(|f| f.path.clone()));
            for f in &s.filters {
                event(shared, BrokerEvent::Subscribed(f.path.clone()));
            }
            let _ = tx.send(encode_v5(v5::Packet::SubAck(v5::SubAck { pkid: s.pkid, return_codes: codes, properties: None })));
            for f in &s.filters {
                replay(shared, tx, &f.path);
            }
        }
        v5::Packet::Unsubscribe(u) => {
            shared.subscriptions.lock().unwrap().retain(|f| !u.filters.contains(f));
            shared.active.lock().unwrap().retain(|f| !u.filters.contains(f));
            for f in &u.filters {
                event(shared, BrokerEvent::Unsubscribed(f.clone()));
            }
            let reasons = u.filters.iter().map(|_| v5::UnsubAckReason::Success).collect();
            let _ = tx.send(encode_v5(v5::Packet::UnsubAck(v5::UnsubAck { pkid: u.pkid, reasons, properties: None })));
        }
        v5::Packet::Publish(p) => {
            let topic = String::from_utf8_lossy(&p.topic).into_owned();
            let received = Received {
                topic: topic.clone(),
                payload: p.payload.clone(),
                qos: p.qos,
                retain: p.retain,
                properties: p.properties.clone(),
            };
            record(shared, received);
            route(shared, tx, &topic, &p.payload, p.retain, p.properties.clone());
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
        v5::Packet::Disconnect(_) => {
            event(shared, BrokerEvent::Disconnected);
            return Some(Read::Closed);
        }
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
            event(shared, BrokerEvent::Connected);
            let _ = tx.send(encode_v4(v4::Packet::ConnAck(v4::ConnAck::new(v4::ConnectReturnCode::Success, false))));
        }
        v4::Packet::Subscribe(s) => {
            let codes = s.filters.iter().map(|f| v4::SubscribeReasonCode::Success(f.qos)).collect();
            shared.subscriptions.lock().unwrap().extend(s.filters.iter().map(|f| f.path.clone()));
            shared.active.lock().unwrap().extend(s.filters.iter().map(|f| f.path.clone()));
            for f in &s.filters {
                event(shared, BrokerEvent::Subscribed(f.path.clone()));
            }
            let _ = tx.send(encode_v4(v4::Packet::SubAck(v4::SubAck::new(s.pkid, codes))));
            for f in &s.filters {
                replay(shared, tx, &f.path);
            }
        }
        v4::Packet::Unsubscribe(u) => {
            shared.subscriptions.lock().unwrap().retain(|f| !u.topics.contains(f));
            shared.active.lock().unwrap().retain(|f| !u.topics.contains(f));
            for f in &u.topics {
                event(shared, BrokerEvent::Unsubscribed(f.clone()));
            }
            let _ = tx.send(encode_v4(v4::Packet::UnsubAck(v4::UnsubAck::new(u.pkid))));
        }
        v4::Packet::Publish(p) => {
            let qos = from_v4_qos(p.qos);
            let payload = Bytes::copy_from_slice(&p.payload);
            record(shared, Received { topic: p.topic.clone(), payload: payload.clone(), qos, retain: p.retain, properties: None });
            route(shared, tx, &p.topic, &payload, p.retain, None);
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
        v4::Packet::Disconnect => {
            event(shared, BrokerEvent::Disconnected);
            return Some(Read::Closed);
        }
        _ => {}
    }
    Some(Read::Handled)
}

#[cfg(test)]
mod tests;

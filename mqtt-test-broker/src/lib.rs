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
//! - **Another client's publish:** [`FakeBroker::publish`] publishes as another client would —
//!   kept when retained, and delivered to the connected client only through a matching
//!   subscription of its current connection, at QoS 1. [`FakeBroker::send_with`] is the raw form:
//!   straight to the client at QoS 0, whatever it subscribed to, flagged as the test says.
//! - **v5 properties both ways:** a publish the broker sends carries a message expiry, user
//!   properties, a response topic and correlation data ([`SendOptions`]); a publish received keeps
//!   its properties ([`Received::properties`]).
//! - **Retained messages:** a retained publish from the client is kept (an empty one clears its
//!   topic), and so is one the test leaves with [`FakeBroker::retain`] — as another client would;
//!   each is replayed at QoS 1, flagged retained and with its remaining expiry, on every
//!   subscription that matches it, after the SUBACK.
//! - **Routing:** a publish from the client goes back to it on its current connection's matching
//!   subscriptions at QoS 1, as a broker forwards it: its retain flag kept only for a subscription
//!   made with retain-as-published, cleared otherwise.
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
//! - **A slow SUBACK:** [`FakeBroker::hold_subacks`] keeps each SUBACK — and the retained messages
//!   its subscription replays after it — until [`FakeBroker::release_subacks`], in order.
//! - **The client's maximum packet size** (v5): a packet larger than its CONNECT says it takes is
//!   discarded, never sent — as MQTT v5 has a broker do.
//! - **The last will** (v5): a connection that ends without a DISCONNECT leaves its CONNECT's will
//!   retained, if the will is retained — as a broker publishes it.
//! - **One slow subscription:** [`FakeBroker::hold_subacks_matching`] holds only the SUBACKs of
//!   subscriptions to a filter with a given prefix, the rest answered at once.
//! - **Several clients** (v5, [`FakeBroker::start_multi`]): for the multi-device host's two
//!   processes on one root — each connection keeps its own subscriptions and will, every publish
//!   reaches every matching subscription, and one connection's end publishes its own will alone.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// One client's connection, in a broker for several clients at once (`FakeBroker::start_multi`).
struct Conn {
    number: usize,
    tx: mpsc::UnboundedSender<BytesMut>,
    /// Its subscriptions, each with whether it was made retain-as-published.
    active: Vec<(String, bool)>,
    /// Its last will: topic, payload, retain. A DISCONNECT clears it.
    will: Option<(String, Bytes, bool)>,
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
    /// How long each QoS 1 ack is delayed, in milliseconds (0: none): a broker slow but steady.
    ack_delay_ms: AtomicU64,
    /// The delayed acks' line, for the connection whose writer it holds: one task sends them in
    /// the order they were decided, each at its time, so a delay changed mid-test reorders nothing.
    ack_line: Mutex<Option<AckLine>>,
    /// The current connection's writer, taking encoded packets.
    to_client: Mutex<Option<mpsc::UnboundedSender<BytesMut>>>,
    /// Any change a test may wait for: a publish received, a subscription, a connection.
    changed: Notify,
    /// The retained messages, by topic.
    retained: Mutex<BTreeMap<String, Retained>>,
    /// The current connection's subscriptions, each with whether it was made retain-as-published
    /// (`subscriptions` keeps every filter ever subscribed).
    active: Mutex<Vec<(String, bool)>>,
    /// The next packet id of a QoS 1 publish to the client.
    next_pkid: std::sync::atomic::AtomicU16,
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
    /// The current connection's last will (v5), by connection number: its topic, payload and
    /// retain flag. A DISCONNECT clears it.
    will: Mutex<Option<(usize, String, Bytes, bool)>>,
    /// The current connection's maximum packet size (v5 CONNECT), by connection number.
    max_packet: Mutex<Option<(usize, u32)>>,
    /// Whether SUBACKs (and their replays) are held, and those held, in order — each with the
    /// writer of the connection it is for.
    holding_subacks: AtomicBool,
    held_subacks: Mutex<Vec<(mpsc::UnboundedSender<BytesMut>, BytesMut)>>,
    /// SUBACKs of a subscription to a filter starting with one of these are held
    /// (`hold_subacks_matching`), the others answered.
    held_prefixes: Mutex<Vec<String>>,
    /// Several clients at once (`start_multi`): each connection keeps its own subscriptions and
    /// will, in `conns`, and a publish goes to every connection with a matching subscription.
    multi: bool,
    conns: Mutex<Vec<Conn>>,
    /// Filters starting with one of these are refused, `NotAuthorized` (an ACL).
    denied: Mutex<Vec<String>>,
    /// The client's packet ids whose acknowledgement is held — a SUBACK or an UNSUBACK — by
    /// connection number: in use until released (MQTT §2.2.1).
    pkids_held: Mutex<Vec<(usize, u16)>>,
    /// Every packet the client sent with a packet id still in use (`pkid_reuses`).
    pkid_reuses: Mutex<Vec<String>>,
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

    /// A v5 broker for several clients at once — two bridge processes sharing a root: each
    /// connection keeps its own subscriptions and last will, a publish (the test's or a client's)
    /// reaches every connection with a matching subscription, and one connection's end publishes
    /// its own will alone. The retained messages are shared. The acks are never held per client
    /// (`hold_acks` is for one client); `drop_connection` closes every connection.
    pub async fn start_multi() -> FakeBroker {
        FakeBroker::start_with(Protocol::V5, 10, true).await
    }

    async fn start_protocol(protocol: Protocol, receive_max: u16) -> FakeBroker {
        FakeBroker::start_with(protocol, receive_max, false).await
    }

    async fn start_with(protocol: Protocol, receive_max: u16, multi: bool) -> FakeBroker {
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
            ack_delay_ms: AtomicU64::new(0),
            ack_line: Mutex::new(None),
            to_client: Mutex::new(None),
            changed: Notify::new(),
            retained: Mutex::new(BTreeMap::new()),
            active: Mutex::new(Vec::new()),
            next_pkid: std::sync::atomic::AtomicU16::new(1),
            events: Mutex::new(Vec::new()),
            kick: watch::Sender::new(0),
            cap: AtomicUsize::new(usize::MAX),
            accepted: AtomicUsize::new(0),
            wedged: watch::Sender::new(None),
            current: AtomicUsize::new(0),
            refusing: AtomicBool::new(false),
            will: Mutex::new(None),
            max_packet: Mutex::new(None),
            holding_subacks: AtomicBool::new(false),
            held_subacks: Mutex::new(Vec::new()),
            held_prefixes: Mutex::new(Vec::new()),
            multi,
            conns: Mutex::new(Vec::new()),
            denied: Mutex::new(Vec::new()),
            pkids_held: Mutex::new(Vec::new()),
            pkid_reuses: Mutex::new(Vec::new()),
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
    /// client — raw: whatever it subscribed to. `false` when no client is connected.
    pub fn send_with(&self, topic: &str, payload: impl Into<Bytes>, options: &SendOptions) -> bool {
        let encoded =
            encode_publish(self.shared.protocol, topic, payload.into(), options.retain, options.properties(options.message_expiry), None);
        match &*self.shared.to_client.lock().unwrap() {
            Some(tx) => tx.send(encoded).is_ok(),
            None => false,
        }
    }

    /// Publish as another client would: kept when `options.retain` says so (an empty payload clears
    /// the topic), and delivered to the connected client if a subscription of its current
    /// connection matches — at QoS 1, flagged retained only for a subscription made
    /// retain-as-published. Returns whether it was delivered.
    pub fn publish(&self, topic: &str, payload: impl Into<Bytes>, options: &SendOptions) -> bool {
        let payload = payload.into();
        if options.retain {
            store_retained(&self.shared, topic, payload.clone(), options.clone());
        }
        if self.shared.multi {
            let mut delivered = false;
            for (tx, preserve_retain) in matching_conns(&self.shared, topic) {
                let encoded = encode_publish(
                    self.shared.protocol,
                    topic,
                    payload.clone(),
                    options.retain && preserve_retain,
                    options.properties(options.message_expiry),
                    Some(next_pkid(&self.shared)),
                );
                delivered |= tx.send(encoded).is_ok();
            }
            return delivered;
        }
        let Some(preserve_retain) = matching(&self.shared, topic) else {
            return false;
        };
        let encoded = encode_publish(
            self.shared.protocol,
            topic,
            payload,
            options.retain && preserve_retain,
            options.properties(options.message_expiry),
            Some(next_pkid(&self.shared)),
        );
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
        // A restarted broker forgets the connection's will with everything else.
        *self.shared.will.lock().unwrap() = None;
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

    /// Acknowledge each of the client's QoS 1 publishes `delay` after it arrives, from now on (zero:
    /// at once): a broker slow but steady — the acks keep coming, so nothing stalls, but a client
    /// whose in-flight window fills publishes no faster than they do. The delayed acks go out in
    /// the order their publishes came, whatever the delay was for each.
    pub fn delay_acks(&self, delay: Duration) {
        self.shared.ack_delay_ms.store(delay.as_millis() as u64, Ordering::SeqCst);
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
                send_ack(&self.shared, tx, puback(self.shared.protocol, pkid), Duration::ZERO);
            }
        }
    }

    /// Hold every SUBACK from now on, and the retained messages its subscription replays after it
    /// (the subscription itself is made): a broker slow to acknowledge a subscription.
    pub fn hold_subacks(&self) {
        self.shared.holding_subacks.store(true, Ordering::SeqCst);
    }

    /// Refuse every subscription to a filter starting with `prefix` from now on, with the reason
    /// code `NotAuthorized`: no subscription is made and nothing replayed (a broker's ACL).
    pub fn deny_subscriptions(&self, prefix: &str) {
        self.shared.denied.lock().unwrap().push(prefix.to_owned());
    }

    /// Hold, from now on, the SUBACK (and replay) of every subscription to a filter starting with
    /// `prefix` — the others are answered at once, out of order with the held one, as a broker that
    /// is slow on one subscription. Released by [`release_subacks`](Self::release_subacks).
    pub fn hold_subacks_matching(&self, prefix: &str) {
        self.shared.held_prefixes.lock().unwrap().push(prefix.to_owned());
    }

    /// Send the SUBACKs held, and their replays, in order, and hold none from now on.
    pub fn release_subacks(&self) {
        // Sent under the lock a SUBSCRIBE takes to decide whether to hold its SUBACK: one coming
        // meanwhile goes out after these, as the broker answers in order (Codex final gate).
        let mut held = self.shared.held_subacks.lock().unwrap();
        self.shared.holding_subacks.store(false, Ordering::SeqCst);
        self.shared.held_prefixes.lock().unwrap().clear();
        self.shared.pkids_held.lock().unwrap().clear();
        for (tx, packet) in std::mem::take(&mut *held) {
            let _ = tx.send(packet);
        }
    }

    /// Every packet the client sent — a QoS 1 or 2 PUBLISH, a SUBSCRIBE, an UNSUBSCRIBE — whose
    /// packet id was still in use by one of its SUBSCRIBEs or UNSUBSCRIBEs whose acknowledgement
    /// the broker holds (MQTT §2.2.1: an id is not reused until its acknowledgement), each said as
    /// text. A real broker may refuse such a packet (`0x91`, packet identifier in use).
    pub fn pkid_reuses(&self) -> Vec<String> {
        self.shared.pkid_reuses.lock().unwrap().clone()
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
    if shared.multi {
        // One client among several: a connection of its own, the others untouched.
        shared.conns.lock().unwrap().push(Conn { number, tx: tx.clone(), active: Vec::new(), will: None });
    } else {
        lock_held(&shared).clear();
        // A new connection starts without a session: no subscription carries over (the R1
        // re-gate). Nor does a SUBACK or a replay held for an earlier connection: each held packet
        // carries the sender of the connection that asked, so a release after a reconnect sends it
        // nowhere — in either mode, as several clients need.
        shared.active.lock().unwrap().clear();
    }
    let mut kick = shared.kick.subscribe();
    kick.mark_unchanged();
    shared.current.store(number, Ordering::SeqCst);
    let mut wedged = shared.wedged.subscribe();
    // The writer goes with this task: when the client goes, and when the task is aborted (the
    // broker dropped) — also while the writer is blocked in `write_all` on a client that stopped
    // reading, which a detached writer survived, holding the client's socket open (review
    // finding T4).
    let limits = shared.clone();
    let _writer = AbortOnDrop(tokio::spawn(async move {
        while let Some(packet) = rx.recv().await {
            // A packet past the client's maximum is discarded (MQTT v5 §3.1.2.11.4).
            let max = *limits.max_packet.lock().unwrap();
            if max.is_some_and(|(n, max)| n == number && packet.len() > max as usize) {
                continue;
            }
            if wr.write_all(&packet).await.is_err() {
                break;
            }
        }
    }));
    let mut buf = BytesMut::with_capacity(8 * 1024);
    'read: loop {
        if *wedged.borrow() == Some(number) {
            // The receive buffer pinned at its size: an operating system that auto-tunes it (macOS
            // does) would otherwise grow it under a zero window, and the client's blocked write
            // would go on after its persist probe, about 5 s later — no broker that stopped reading
            // (the pump test that ended at 15 s instead of 10).
            let _ = socket2::SockRef::from(rd.as_ref()).set_recv_buffer_size(WEDGED_RECV_BUFFER);
        }
        while *wedged.borrow_and_update() == Some(number) {
            // Wedged (`stop_reading`): nothing more of this connection is read or handled.
            tokio::select! {
                _ = wedged.changed() => {}
                _ = kick.changed() => break 'read,
            }
        }
        loop {
            let read = match shared.protocol {
                Protocol::V5 => read_v5(&mut buf, &tx, &shared, number),
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
    if shared.multi {
        let conn = {
            let mut conns = shared.conns.lock().unwrap();
            conns.iter().position(|c| c.number == number).map(|i| conns.remove(i))
        };
        // Its will, kept when retained and delivered to the others' matching subscriptions.
        if let Some((topic, payload, retain)) = conn.and_then(|c| c.will) {
            if retain {
                store_retained(&shared, &topic, payload.clone(), SendOptions { retain: true, ..SendOptions::default() });
            }
            for (tx, preserve_retain) in matching_conns(&shared, &topic) {
                let _ = tx.send(encode_publish(
                    shared.protocol,
                    &topic,
                    payload.clone(),
                    retain && preserve_retain,
                    None,
                    Some(next_pkid(&shared)),
                ));
            }
        }
    } else {
        publish_will(&shared, number);
    }
    shared.changed.notify_waiters();
}

/// A connection's writer, and the line of delayed acks — each with when it is due — that one task
/// sends to it in order.
type AckLine = (mpsc::UnboundedSender<BytesMut>, mpsc::UnboundedSender<(tokio::time::Instant, BytesMut)>);

/// The receive buffer a wedged connection is pinned to (see `connection`).
const WEDGED_RECV_BUFFER: usize = 64 * 1024;

/// A clean DISCONNECT: the connection's will is not published.
fn clear_will(shared: &Shared, number: usize) {
    if shared.multi {
        if let Some(conn) = shared.conns.lock().unwrap().iter_mut().find(|c| c.number == number) {
            conn.will = None;
        }
        return;
    }
    let mut will = shared.will.lock().unwrap();
    if will.as_ref().is_some_and(|(n, ..)| *n == number) {
        *will = None;
    }
}

/// The connection ended: its will, if it has one still (no DISCONNECT came), is published — kept
/// when retained.
fn publish_will(shared: &Shared, number: usize) {
    let will = {
        let mut will = shared.will.lock().unwrap();
        match will.as_ref() {
            Some((n, ..)) if *n == number => will.take(),
            _ => None,
        }
    };
    if let Some((_, topic, payload, retain)) = will {
        if retain {
            store_retained(shared, &topic, payload, SendOptions { retain: true, ..SendOptions::default() });
        }
    }
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

/// Whether a subscription of the current connection matches `topic`: `Some(retain-as-published)`
/// of the first that does.
fn matching(shared: &Shared, topic: &str) -> Option<bool> {
    let active = shared.active.lock().unwrap();
    active.iter().find(|(filter, _)| rumqttc::v5::mqttbytes::matches(topic, filter)).map(|(_, preserve)| *preserve)
}

/// In a broker for several clients: every connection with a subscription matching `topic`, its
/// writer and the retain-as-published of its first matching subscription.
fn matching_conns(shared: &Shared, topic: &str) -> Vec<(mpsc::UnboundedSender<BytesMut>, bool)> {
    let conns = shared.conns.lock().unwrap();
    conns
        .iter()
        .filter_map(|c| {
            c.active
                .iter()
                .find(|(filter, _)| rumqttc::v5::mqttbytes::matches(topic, filter))
                .map(|(_, preserve)| (c.tx.clone(), *preserve))
        })
        .collect()
}

/// The subscriptions of connection `number`: the broker's one list, or — for several clients — the
/// connection's own.
fn with_active<T>(shared: &Shared, number: usize, f: impl FnOnce(&mut Vec<(String, bool)>) -> T) -> T {
    if shared.multi {
        let mut conns = shared.conns.lock().unwrap();
        match conns.iter_mut().find(|c| c.number == number) {
            Some(conn) => f(&mut conn.active),
            None => f(&mut Vec::new()),
        }
    } else {
        f(&mut shared.active.lock().unwrap())
    }
}

/// The next packet id of a QoS 1 publish to the client (never 0).
fn next_pkid(shared: &Shared) -> u16 {
    loop {
        let pkid = shared.next_pkid.fetch_add(1, Ordering::SeqCst);
        if pkid != 0 {
            return pkid;
        }
    }
}

/// A publish from the client: kept if retained, then forwarded to the client's own subscriptions
/// that match it, once, at QoS 1 — flagged retained only for a retain-as-published subscription.
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
    if shared.multi {
        for (to, preserve_retain) in matching_conns(shared, topic) {
            let _ = to.send(encode_publish(
                shared.protocol,
                topic,
                payload.clone(),
                retain && preserve_retain,
                properties.clone(),
                Some(next_pkid(shared)),
            ));
        }
        return;
    }
    if let Some(preserve_retain) = matching(shared, topic) {
        let _ = tx.send(encode_publish(
            shared.protocol,
            topic,
            payload.clone(),
            retain && preserve_retain,
            properties,
            Some(next_pkid(shared)),
        ));
    }
}

/// After a SUBACK: the retained messages matching `filter`, at QoS 1, flagged retained, each with
/// what is left of its expiry (an expired one is dropped).
fn replay(shared: &Shared, filter: &str) -> Vec<BytesMut> {
    let mut retained = shared.retained.lock().unwrap();
    retained.retain(|_, r| r.remaining().is_some());
    let mut packets = Vec::new();
    for (topic, r) in retained.iter() {
        if rumqttc::v5::mqttbytes::matches(topic, filter) {
            let expiry = r.remaining().flatten();
            packets.push(encode_publish(
                shared.protocol,
                topic,
                r.payload.clone(),
                true,
                r.options.properties(expiry),
                Some(next_pkid(shared)),
            ));
        }
    }
    packets
}

/// A publish to the client: QoS 1 with `pkid`, or QoS 0 without one.
fn encode_publish(
    protocol: Protocol,
    topic: &str,
    payload: Bytes,
    retain: bool,
    properties: Option<PublishProperties>,
    pkid: Option<u16>,
) -> BytesMut {
    let qos = if pkid.is_some() { QoS::AtLeastOnce } else { QoS::AtMostOnce };
    match protocol {
        Protocol::V5 => {
            let mut publish = v5::Publish::new(topic, qos, payload, properties);
            publish.retain = retain;
            publish.pkid = pkid.unwrap_or(0);
            encode_v5(v5::Packet::Publish(publish))
        }
        Protocol::V4 => {
            let mut publish = v4::Publish::new(topic, v4_qos(qos), payload.to_vec());
            publish.retain = retain;
            publish.pkid = pkid.unwrap_or(0);
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
    let delay = shared.ack_delay_ms.load(Ordering::SeqCst);
    if shared.hold_acks.load(Ordering::SeqCst) {
        held.push(pkid);
    } else {
        send_ack(shared, tx, puback(shared.protocol, pkid), Duration::from_millis(delay));
    }
}

/// Send an ack `delay` from now. Once a connection has a line of delayed acks, every later ack of
/// it — at no delay, or released — goes through that line too, so none overtakes one delayed
/// before it (the confirming review's minor).
fn send_ack(shared: &Shared, tx: &mpsc::UnboundedSender<BytesMut>, packet: BytesMut, delay: Duration) {
    let mut line = shared.ack_line.lock().unwrap();
    let has_line = line.as_ref().is_some_and(|(writer, _)| writer.same_channel(tx));
    if !has_line && delay.is_zero() {
        let _ = tx.send(packet);
        return;
    }
    if !has_line {
        let (queue, mut queued) = mpsc::unbounded_channel::<(tokio::time::Instant, BytesMut)>();
        let writer = tx.clone();
        tokio::spawn(async move {
            while let Some((due, packet)) = queued.recv().await {
                tokio::time::sleep_until(due).await;
                let _ = writer.send(packet);
            }
        });
        *line = Some((tx.clone(), queue));
    }
    if let Some((_, queue)) = line.as_ref() {
        let _ = queue.send((tokio::time::Instant::now() + delay, packet));
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

/// A packet of connection `number` carrying `pkid`: recorded as a reuse if one of the connection's
/// SUBSCRIBEs or UNSUBSCRIBEs whose acknowledgement is held has that id (MQTT §2.2.1).
fn check_pkid(shared: &Shared, number: usize, pkid: u16, what: &str) {
    if shared.pkids_held.lock().unwrap().contains(&(number, pkid)) {
        shared.pkid_reuses.lock().unwrap().push(format!("{what} with packet id {pkid}, in use by a request whose acknowledgement is held"));
    }
}

/// Read and answer one v5 packet; `None` on a malformed stream.
fn read_v5(buf: &mut BytesMut, tx: &mpsc::UnboundedSender<BytesMut>, shared: &Shared, number: usize) -> Option<Read> {
    // A DISCONNECT with reason 0 and no properties has a remaining length of 0 (MQTT v5 §3.14.2.1),
    // as rumqttc's own client writes it — and rumqttc 0.25's reader refuses it (`PayloadRequired`).
    if buf.starts_with(&[0xE0, 0x00]) {
        let _ = buf.split_to(2);
        clear_will(shared, number);
        event(shared, BrokerEvent::Disconnected);
        return Some(Read::Closed);
    }
    let packet = match v5::Packet::read(buf, None) {
        Ok(p) => p,
        Err(rumqttc::v5::mqttbytes::Error::InsufficientBytes(_)) => return Some(Read::NeedMore),
        Err(_) => return None,
    };
    match packet {
        v5::Packet::Connect(connect, will, _) => {
            *shared.max_packet.lock().unwrap() = connect.properties.as_ref().and_then(|p| p.max_packet_size).map(|max| (number, max));
            if shared.multi {
                if let Some(conn) = shared.conns.lock().unwrap().iter_mut().find(|c| c.number == number) {
                    conn.will = will.map(|w| (String::from_utf8_lossy(&w.topic).into_owned(), w.message.clone(), w.retain));
                }
            } else {
                *shared.will.lock().unwrap() =
                    will.map(|w| (number, String::from_utf8_lossy(&w.topic).into_owned(), w.message.clone(), w.retain));
            }
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
            check_pkid(shared, number, s.pkid, "SUBSCRIBE");
            let denied = |path: &str| shared.denied.lock().unwrap().iter().any(|prefix| path.starts_with(prefix.as_str()));
            let codes = s
                .filters
                .iter()
                .map(|f| if denied(&f.path) { v5::SubscribeReasonCode::NotAuthorized } else { v5::SubscribeReasonCode::Success(f.qos) })
                .collect();
            let granted: Vec<&v5::Filter> = s.filters.iter().filter(|f| !denied(&f.path)).collect();
            shared.subscriptions.lock().unwrap().extend(granted.iter().map(|f| f.path.clone()));
            with_active(shared, number, |active| active.extend(granted.iter().map(|f| (f.path.clone(), f.preserve_retain))));
            for f in &granted {
                event(shared, BrokerEvent::Subscribed(f.path.clone()));
            }
            let mut packets = vec![encode_v5(v5::Packet::SubAck(v5::SubAck { pkid: s.pkid, return_codes: codes, properties: None }))];
            for f in &granted {
                packets.extend(replay(shared, &f.path));
            }
            let slow = {
                let prefixes = shared.held_prefixes.lock().unwrap();
                s.filters.iter().any(|f| prefixes.iter().any(|p| f.path.starts_with(p.as_str())))
            };
            let mut held = shared.held_subacks.lock().unwrap();
            if shared.holding_subacks.load(Ordering::SeqCst) || slow {
                shared.pkids_held.lock().unwrap().push((number, s.pkid));
                held.extend(packets.into_iter().map(|p| (tx.clone(), p)));
            } else {
                for packet in packets {
                    let _ = tx.send(packet);
                }
            }
        }
        v5::Packet::Unsubscribe(u) => {
            check_pkid(shared, number, u.pkid, "UNSUBSCRIBE");
            {
                let mut subscriptions = shared.subscriptions.lock().unwrap();
                if shared.multi {
                    // One client's subscription, taken off the list once: another client's stays.
                    for f in &u.filters {
                        if let Some(i) = subscriptions.iter().position(|s| s == f) {
                            subscriptions.remove(i);
                        }
                    }
                } else {
                    subscriptions.retain(|f| !u.filters.contains(f));
                }
            }
            with_active(shared, number, |active| active.retain(|(f, _)| !u.filters.contains(f)));
            for f in &u.filters {
                event(shared, BrokerEvent::Unsubscribed(f.clone()));
            }
            let reasons = u.filters.iter().map(|_| v5::UnsubAckReason::Success).collect();
            let unsuback = encode_v5(v5::Packet::UnsubAck(v5::UnsubAck { pkid: u.pkid, reasons, properties: None }));
            // Behind any SUBACK held: a broker answers in order (Claude's final gate minor 2).
            let mut held = shared.held_subacks.lock().unwrap();
            if shared.holding_subacks.load(Ordering::SeqCst) {
                shared.pkids_held.lock().unwrap().push((number, u.pkid));
                held.push((tx.clone(), unsuback));
            } else {
                let _ = tx.send(unsuback);
            }
        }
        v5::Packet::Publish(p) => {
            if p.qos != QoS::AtMostOnce {
                check_pkid(shared, number, p.pkid, "PUBLISH");
            }
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
            clear_will(shared, number);
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
            shared.active.lock().unwrap().extend(s.filters.iter().map(|f| (f.path.clone(), false)));
            for f in &s.filters {
                event(shared, BrokerEvent::Subscribed(f.path.clone()));
            }
            let _ = tx.send(encode_v4(v4::Packet::SubAck(v4::SubAck::new(s.pkid, codes))));
            for f in &s.filters {
                for packet in replay(shared, &f.path) {
                    let _ = tx.send(packet);
                }
            }
        }
        v4::Packet::Unsubscribe(u) => {
            shared.subscriptions.lock().unwrap().retain(|f| !u.topics.contains(f));
            shared.active.lock().unwrap().retain(|(f, _)| !u.topics.contains(f));
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

//! rumqttc's event loop, polled by a task that waits on nothing else (lifted from mqtt_ac's
//! `mqtt_pump.rs`; Store no-hang §14.3, §14.6).
//!
//! rumqttc's request channel drains only while the event loop is polled. The session waits on the
//! request channel (its publishes and subscribes), so a poller that did any of that work could wait
//! on itself. So the pump polls, and forwards what arrives on an unbounded queue — with a
//! high-water episode, since growth is a symptom to see — and the session does the rest.
//! With opt-in buffering, every queued PUBLISH retains a byte reservation. A full
//! incoming budget ends the connection immediately; the pump never waits for space.
//!
//! **One connection per event loop.** The pump never lets rumqttc reconnect inside an event loop:
//! the first failed poll ends the session, and the MQTT worker makes a fresh client for the next
//! connection. A reconnect inside one event loop would send whatever the session had handed the
//! request channel meanwhile ahead of the next CONNACK's subscriptions (review MINOR 14), and would
//! carry a packet-id collision across the connections (rumqttc 0.25's `MqttState::clean` leaves it
//! standing, and the event loop then takes no request).
//!
//! **Progress, bounded** (review C1). Nothing in rumqttc bounds how long the broker may go without
//! progress: a broker that answers PINGREQ but withholds its acknowledgements fills the in-flight
//! window, and rumqttc then takes no request while its pings go on succeeding; a broker that stops
//! reading leaves rumqttc in its flush, inside `poll`, where its keep-alive cannot fire. So the pump
//! ends the session when a request has been outstanding for [`stall_bound`] (twice the keep-alive)
//! with no acknowledgement of any, and when one poll has not returned within it — dropping the
//! event loop rather than polling a cancelled one again. A PINGRESP or a publish from the broker is
//! not progress on what the bridge asked.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{Packet, PubAckReason, Publish, SubscribeReasonCode, UnsubAckReason};
use rumqttc::v5::{Event, EventLoop};
use rumqttc::Outgoing;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tracing::{debug, info, warn, Instrument};

/// Failed connects in a row, each a session of its own, after which the MQTT worker pauses for
/// `MqttTiming::retry` rather than `MqttTiming::poll_retry` before the next.
pub const MAX_CONSECUTIVE_ERRORS: u32 = 5;

/// A forward queue past this many unread publishes is a WARN, once; back under [`LOW_WATER`], an
/// INFO with how long it lasted. Nothing is dropped (Store no-hang §14.6, ruling 1).
pub const HIGH_WATER: usize = 1000;
/// The forward queue's unread count under which a backlog episode ends.
pub const LOW_WATER: usize = 100;

/// How long a request may stay outstanding with nothing acknowledged, and how long one poll may
/// take, before the pump ends the session: twice the keep-alive — as long as rumqttc gives a
/// PINGREQ its answer. A poll returns at least once per keep-alive while connected (its ping), and
/// a connect is bounded by rumqttc's connection timeout (5 s), under the 10 s this gives at the
/// smallest keep-alive rumqttc takes.
pub fn stall_bound(keep_alive: Duration) -> Duration {
    keep_alive * 2
}

/// What the pump hands the session. A value lives on the forward queue only until the session
/// reads it; boxing the publish would cost an allocation for each.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum PumpEvent {
    /// The broker accepted the connection. The broker keeps no session, so the session subscribes
    /// and announces itself.
    Connected,
    /// Exact outgoing publish sequence and the packet id assigned by rumqttc.
    Publishing {
        /// Publish ordinal on this connection, including ordinary traffic.
        sequence: u64,
        /// Packet identifier actually assigned to this publish.
        pkid: u16,
    },
    /// Exact QoS 1 acknowledgment, including broker refusal.
    PubAck {
        /// Packet identifier acknowledged on this connection.
        pkid: u16,
        /// Success or a negative MQTT reason code.
        result: Result<(), String>,
    },
    /// A publish, and when the pump received it: a request's expiry counts from then, not from
    /// when the session gets to it (review C7).
    Publish(Publish, Instant),
    /// A SUBSCRIBE was written, with its packet id: the session tells the read-back's SUBACK by it.
    Subscribing(u16),
    /// The broker answered a subscription: its packet id, and whether it granted every filter (an
    /// ACL may refuse one).
    SubAck {
        /// The SUBSCRIBE's packet id.
        pkid: u16,
        /// Whether every filter was granted.
        granted: bool,
    },
    /// An UNSUBSCRIBE was written, with its packet id: the multi-device host correlates a device's
    /// unsubscriptions by it (multi-device spec §4.4 step 4; an R2 extension of the pump).
    Unsubscribing(u16),
    /// The broker answered an unsubscription: its packet id, and whether every filter's reason was
    /// a success (`NoSubscriptionExisted` counts as one: the filter is gone either way).
    UnsubAck {
        /// The UNSUBSCRIBE's packet id.
        pkid: u16,
        /// Whether every filter's reason was a success.
        granted: bool,
    },
    /// The DISCONNECT the session asked for was written: the session's end.
    Disconnected,
    /// The connection failed or stalled, and its event loop is dropped: the session ends and the
    /// MQTT worker makes a fresh one.
    Ended(String),
}

pub(crate) struct Queued {
    event: PumpEvent,
    _charge: Option<crate::buffering::Charge>,
}
impl From<PumpEvent> for Queued {
    fn from(event: PumpEvent) -> Self {
        Self { event, _charge: None }
    }
}

/// The forward queue's depth, and whether its high-water WARN is standing. Atomics only, so
/// neither the pump nor the session ever waits on it, and nothing is logged under a lock.
struct Backlog {
    depth: AtomicUsize,
    /// When the standing episode began, in milliseconds since `epoch` plus one; 0 when none stands.
    high_since: AtomicU64,
    epoch: Instant,
}

impl Backlog {
    fn new() -> Backlog {
        Backlog { depth: AtomicUsize::new(0), high_since: AtomicU64::new(0), epoch: Instant::now() }
    }

    fn stamp(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    /// The pump is about to forward a publish.
    fn pushed(&self) {
        let depth = self.depth.fetch_add(1, Ordering::SeqCst) + 1;
        if depth >= HIGH_WATER && self.high_since.compare_exchange(0, self.stamp(), Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            warn!(
                kind = "mqtt_backlog_high",
                depth,
                high_water = HIGH_WATER,
                "MQTT requests are arriving faster than the bridge handles them"
            );
        }
    }

    /// The session took a publish. Saturating: a count gone wrong must not panic the session.
    fn popped(&self) {
        // A compare-exchange loop rather than `fetch_update`, deprecated from Rust 1.99.
        let mut before = self.depth.load(Ordering::SeqCst);
        while let Err(now) = self.depth.compare_exchange_weak(before, before.saturating_sub(1), Ordering::SeqCst, Ordering::SeqCst) {
            before = now;
        }
        let depth = before.saturating_sub(1);
        if depth <= LOW_WATER {
            self.close(depth, false);
        }
    }

    /// End a standing high-water episode with its INFO: the queue drained, or its session ended.
    fn close(&self, depth: usize, session_ended: bool) {
        let since = self.high_since.swap(0, Ordering::SeqCst);
        if since != 0 {
            let lasted_ms = self.stamp().saturating_sub(since);
            info!(kind = "mqtt_backlog_drained", depth, lasted_ms, session_ended, "MQTT request backlog drained");
        }
    }
}

/// The broker's reachability as the pumps see it. It outlives every session — each connection is
/// a session of its own — so an outage is one episode however many attempts and sessions it takes
/// (the logging policy): an INFO `connection_lost` when it starts, one WARN `external_failure` once
/// it has lasted `warn_after`, the attempts in between at DEBUG, and an INFO `external_recovered`
/// with its length once a connection has lasted `stable_after` — a connection lost sooner is one
/// more failed attempt of the same outage (the fleet's F2: recovery on proof of life, never on a
/// bare CONNACK). Atomics only: one pump at a time uses it, and nothing is logged under a lock.
pub struct BrokerOutage {
    warn_after: Duration,
    stable_after: Duration,
    epoch: Instant,
    /// When the outage began, in milliseconds since `epoch` plus one; 0 while the broker is
    /// reachable.
    since: AtomicU64,
    /// When the broker accepted the connection that may end the outage, the same way; 0 when none
    /// is on trial.
    connected_at: AtomicU64,
    attempts: AtomicU32,
    warned: AtomicBool,
}

impl BrokerOutage {
    /// Episode tracking with the WARN threshold and the stability window of [`crate::session::MqttTiming`].
    pub fn new(warn_after: Duration, stable_after: Duration) -> BrokerOutage {
        BrokerOutage {
            warn_after,
            stable_after,
            epoch: Instant::now(),
            since: AtomicU64::new(0),
            connected_at: AtomicU64::new(0),
            attempts: AtomicU32::new(0),
            warned: AtomicBool::new(false),
        }
    }

    fn stamp(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }

    /// A connection was lost, a connect failed, or the broker stalled. A connection on trial lost
    /// is one more attempt of its outage.
    fn failed(&self, error: &str) {
        let now = self.stamp();
        self.connected_at.store(0, Ordering::SeqCst);
        if self.since.compare_exchange(0, now, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            self.attempts.store(1, Ordering::SeqCst);
            self.warned.store(false, Ordering::SeqCst);
            info!(kind = "connection_lost", error, "MQTT broker connection lost; reconnecting");
            return;
        }
        let attempts = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        let down_for_ms = now.saturating_sub(self.since.load(Ordering::SeqCst));
        let past_threshold = down_for_ms >= self.warn_after.as_millis() as u64;
        if past_threshold && !self.warned.swap(true, Ordering::SeqCst) {
            warn!(kind = "external_failure", error, attempts, down_for_ms, "MQTT broker unreachable; still reconnecting");
        } else {
            debug!(error, attempts, down_for_ms, "MQTT broker still unreachable");
        }
    }

    /// The broker accepted a connection. Outside an outage that is all; during one, the connection
    /// is on trial until it has lasted `stable_after` (`alive`).
    fn connected(&self) {
        if self.since.load(Ordering::SeqCst) == 0 {
            info!("Connected to the MQTT broker");
            return;
        }
        self.connected_at.store(self.stamp(), Ordering::SeqCst);
        debug!(attempts = self.attempts.load(Ordering::SeqCst), "MQTT broker accepted a connection; the outage ends once it lasts");
    }

    /// The broker sent something on the current connection. Once a connection on trial has lasted
    /// `stable_after`, the outage is over: an INFO with its length, up to that connection.
    fn alive(&self) {
        let connected_at = self.connected_at.load(Ordering::SeqCst);
        if connected_at == 0 || self.stamp().saturating_sub(connected_at) < self.stable_after.as_millis() as u64 {
            return;
        }
        self.connected_at.store(0, Ordering::SeqCst);
        let since = self.since.swap(0, Ordering::SeqCst);
        if since == 0 {
            return;
        }
        info!(
            kind = "external_recovered",
            attempts = self.attempts.load(Ordering::SeqCst),
            down_for_ms = connected_at.saturating_sub(since),
            "MQTT broker reachable again"
        );
    }

    /// Whether an outage is under way (the shutdown skips its flush then).
    pub fn down(&self) -> bool {
        self.since.load(Ordering::SeqCst) != 0
    }
}

/// What the bridge asked and the broker has not acknowledged yet, and since when nothing has been:
/// the publishes in flight (rumqttc's count) and the subscribes and unsubscribes sent.
#[derive(Debug, Default)]
struct Outstanding {
    subscribes: usize,
    /// While something is outstanding: when it began to be, or the last acknowledgement since.
    since: Option<Instant>,
}

impl Outstanding {
    fn after(&mut self, event: &Event, in_flight: u16) {
        let acknowledged = match event {
            Event::Outgoing(Outgoing::Subscribe(_) | Outgoing::Unsubscribe(_)) => {
                self.subscribes += 1;
                false
            }
            Event::Incoming(Packet::SubAck(_) | Packet::UnsubAck(_)) => {
                self.subscribes = self.subscribes.saturating_sub(1);
                true
            }
            Event::Incoming(Packet::PubAck(_) | Packet::PubRec(_) | Packet::PubComp(_)) => true,
            _ => false,
        };
        self.since = match (in_flight > 0 || self.subscribes > 0, self.since) {
            (false, _) => None,
            (true, Some(_)) if acknowledged => Some(Instant::now()),
            (true, Some(since)) => Some(since),
            (true, None) => Some(Instant::now()),
        };
    }
}

/// Polls rumqttc's event loop in a task of its own. It is given no client, so it cannot publish
/// or subscribe, and its only send is to an unbounded queue: it waits on nothing in this process.
/// Dropping it stops the task, and with it the event loop, so a publish still waiting on the
/// request channel fails instead of waiting for good.
pub struct Pump {
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Pump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// The session's end of the pump's queue.
pub struct Incoming {
    rx: UnboundedReceiver<Queued>,
    backlog: Arc<Backlog>,
    /// The QoS 1 publishes the event loop has written and the broker not yet acknowledged.
    inflight: Arc<AtomicUsize>,
    /// The publishes the event loop has written in this session.
    written: Arc<AtomicU64>,
    /// How many packet ids rumqttc cycles through on this connection (see [`Line::id_space`]).
    ids: Arc<AtomicU16>,
    max_packet: Arc<AtomicU32>,
    /// Told after each event the pump takes, for a wait on its counts.
    progress: Arc<tokio::sync::Notify>,
    /// The session asked for the DISCONNECT the pump will see: it then reads on until the close.
    disconnect_asked: Arc<AtomicBool>,
    /// The pump has read the broker's CONNACK.
    accepted: Arc<AtomicBool>,
    /// The pump has ended — returned, panicked or aborted — and its event loop with it.
    ended: Arc<AtomicBool>,
}

impl Incoming {
    /// Whether the pump holds a connection the broker accepted: its CONNACK read — whether or not
    /// the session has taken it yet — and the pump not ended (third confirming reviews, Claude
    /// minor 1).
    pub fn connected(&self) -> bool {
        self.accepted.load(Ordering::SeqCst) && !self.ended()
    }

    /// Whether the pump has ended: nothing more will be written or acknowledged. Its end wakes a
    /// wait on [`progress`](Self::progress) (Claude minor 2).
    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    /// How many QoS 1 publishes the broker has not acknowledged yet, as of the pump's last event.
    pub fn inflight(&self) -> usize {
        self.inflight.load(Ordering::SeqCst)
    }

    /// How many publishes the event loop has written in this session.
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::SeqCst)
    }

    /// The next event the pump takes. Cancellation-safe; enable it before checking the counts.
    pub fn progress(&self) -> tokio::sync::futures::OwnedNotified {
        self.progress.clone().notified_owned()
    }

    /// The pump's publish counts, as a handle of their own (the session's handlers read them while
    /// the session holds this queue).
    pub fn line(&self) -> Line {
        Line { inflight: self.inflight.clone(), written: self.written.clone(), ids: self.ids.clone(), max_packet: self.max_packet.clone() }
    }

    /// The DISCONNECT the session is about to ask for is its own: the pump reads on after it until
    /// the broker closes, rather than for one rumqttc sends on a protocol error (the second
    /// confirming review's minor 3).
    pub fn expect_disconnect(&self) {
        self.disconnect_asked.store(true, Ordering::SeqCst);
    }

    /// The next event from the pump, waiting for it; `None` once the pump has ended.
    pub async fn recv(&mut self) -> Option<PumpEvent> {
        // WAIT: mqtt-pump-queue
        let event = self.rx.recv().await.map(|queued| queued.event);
        if matches!(event, Some(PumpEvent::Publish(..))) {
            self.backlog.popped();
        }
        event
    }
}

/// The pump's publish counts: whether every publish a session enqueued has been written and
/// acknowledged — the line clear of publishes a host's SUBSCRIBE or UNSUBSCRIBE waits for, so its
/// packet id is in use by nothing else (MQTT §2.2.1; R2 gate, Codex 3).
#[derive(Clone)]
pub struct Line {
    inflight: Arc<AtomicUsize>,
    written: Arc<AtomicU64>,
    ids: Arc<AtomicU16>,
    max_packet: Arc<AtomicU32>,
}

impl Line {
    /// Effective local and broker-advertised ceiling, in complete MQTT packet bytes.
    pub fn maximum_packet_size(&self) -> u32 {
        self.max_packet.load(Ordering::SeqCst).min(crate::session::MAX_PACKET_SIZE)
    }
    /// Whether the event loop has written all `enqueued` publishes and the broker has acknowledged
    /// every one.
    pub fn clear(&self, enqueued: u64) -> bool {
        self.written.load(Ordering::SeqCst) >= enqueued && self.inflight.load(Ordering::SeqCst) == 0
    }

    /// How many of the session's publishes the broker has acknowledged: written, less those in
    /// flight (QoS 1 acknowledgements come in order).
    pub fn acknowledged(&self) -> u64 {
        // The pump increments inflight before written. Read in the opposite
        // order so an outgoing publish cannot look acknowledged mid-update.
        let written = self.written.load(Ordering::SeqCst);
        let inflight = u64::try_from(self.inflight.load(Ordering::SeqCst)).unwrap_or(u64::MAX);
        written.saturating_sub(inflight)
    }

    /// How many packet ids rumqttc 0.25 cycles through on this connection — the broker's receive
    /// maximum from its CONNACK (none: 65535), as rumqttc takes it with no upper limit set — and so
    /// how many ids it hands out, publishes and requests alike, before one comes round again. Zero
    /// before the CONNACK.
    pub fn id_space(&self) -> u16 {
        self.ids.load(Ordering::SeqCst)
    }
}

/// The session is over, and the next one gets a queue of its own: the requests still unread here
/// are dropped. Said once, with how many, and a standing high-water WARN is closed with its INFO
/// rather than left standing for good.
impl Drop for Incoming {
    fn drop(&mut self) {
        let depth = self.backlog.depth.load(Ordering::SeqCst);
        if depth > 0 {
            warn!(kind = "mqtt_commands_discarded", discarded = depth, "MQTT requests left unread when the session ended were discarded");
        }
        self.backlog.close(depth, true);
    }
}

impl Pump {
    /// Start polling; `stall` is [`stall_bound`] of the keep-alive; `on_lost` is called, once, when a
    /// connection the broker accepted is lost or stalls — at once, whatever the session is doing.
    pub fn start(
        events: EventLoop,
        outage: Arc<BrokerOutage>,
        stall: Duration,
        on_lost: impl FnOnce() + Send + 'static,
    ) -> (Pump, Incoming) {
        Self::start_with_receipts(events, outage, stall, on_lost, false)
    }

    pub(crate) fn start_with_receipts(
        events: EventLoop,
        outage: Arc<BrokerOutage>,
        stall: Duration,
        on_lost: impl FnOnce() + Send + 'static,
        receipts: bool,
    ) -> (Pump, Incoming) {
        Self::start_with_buffers(events, outage, stall, on_lost, receipts, None)
    }
    pub(crate) fn start_with_buffers(
        events: EventLoop,
        outage: Arc<BrokerOutage>,
        stall: Duration,
        on_lost: impl FnOnce() + Send + 'static,
        receipts: bool,
        buffers: Option<crate::buffering::Connection>,
    ) -> (Pump, Incoming) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let backlog = Arc::new(Backlog::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(AtomicU64::new(0));
        let ids = Arc::new(AtomicU16::new(0));
        let max_packet = Arc::new(AtomicU32::new(crate::session::MAX_PACKET_SIZE));
        let progress = Arc::new(tokio::sync::Notify::new());
        let disconnect_asked = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let counts = Counts {
            buffers,
            receipts,
            inflight: inflight.clone(),
            written: written.clone(),
            ids: ids.clone(),
            max_packet: max_packet.clone(),
            progress: progress.clone(),
            disconnect_asked: disconnect_asked.clone(),
            accepted: accepted.clone(),
            ended: ended.clone(),
        };
        let task = tokio::spawn(Pump::run(events, tx, backlog.clone(), counts, outage, stall, on_lost).in_current_span());
        (Pump { task }, Incoming { rx, backlog, inflight, written, ids, max_packet, progress, disconnect_asked, accepted, ended })
    }

    async fn run(
        mut events: EventLoop,
        tx: UnboundedSender<Queued>,
        backlog: Arc<Backlog>,
        counts: Counts,
        outage: Arc<BrokerOutage>,
        stall: Duration,
        on_lost: impl FnOnce() + Send + 'static,
    ) {
        let mut outstanding = Outstanding::default();
        let mut on_lost = Some(on_lost);
        let mut connected = false;
        let mut end = |reason: String, connected: bool, broker_failure: bool| {
            if connected {
                if let Some(on_lost) = on_lost.take() {
                    on_lost();
                }
            }
            // Part of the outage's one episode; the event loop goes with this task, and the MQTT
            // worker makes a fresh client.
            if broker_failure {
                outage.failed(&reason);
            }
            debug!(error = %reason, "The MQTT connection ended; its session ends with it");
            let _ = tx.send(PumpEvent::Ended(reason).into());
        };
        loop {
            let polled_at = Instant::now();
            let deadline = match outstanding.since {
                Some(since) => (since + stall).min(polled_at + stall),
                None => polled_at + stall,
            };
            // WAIT: mqtt-poll
            let polled = tokio::time::timeout_at(deadline.into(), events.poll()).await;
            let event = match polled {
                Ok(Ok(event)) => event,
                Ok(Err(e)) => return end(e.to_string(), connected, true),
                Err(_) => {
                    let stall_ms = stall.as_millis() as u64;
                    return end(
                        match outstanding.since {
                        Some(since) if Instant::now() >= since + stall => format!(
                            "the broker acknowledged nothing for {stall_ms} ms with requests outstanding (it may answer pings, but takes no request)"
                        ),
                            _ => format!("a poll did not return within {stall_ms} ms: the broker stopped reading"),
                        },
                        connected,
                        true,
                    );
                }
            };
            outstanding.after(&event, events.state.inflight());
            counts.inflight.store(usize::from(events.state.inflight()), Ordering::SeqCst);
            if matches!(event, Event::Outgoing(Outgoing::Publish(_))) {
                counts.written.fetch_add(1, Ordering::SeqCst);
            }
            counts.progress.notify_waiters();

            // Only an incoming event is proof of the broker; an outgoing one (a connect request)
            // is rumqttc trying.
            if matches!(event, Event::Incoming(_)) {
                outage.alive();
            }

            let forward = match event {
                Event::Incoming(Packet::ConnAck(ack)) => {
                    let receive_max = ack.properties.as_ref().and_then(|p| p.receive_max).unwrap_or(u16::MAX);
                    counts.ids.store(receive_max, Ordering::SeqCst);
                    counts.max_packet.store(
                        ack.properties.as_ref().and_then(|properties| properties.max_packet_size).unwrap_or(u32::MAX),
                        Ordering::SeqCst,
                    );
                    outage.connected();
                    connected = true;
                    counts.accepted.store(true, Ordering::SeqCst);
                    PumpEvent::Connected
                }
                Event::Incoming(Packet::Publish(publish)) => {
                    backlog.pushed();
                    PumpEvent::Publish(publish, Instant::now())
                }
                Event::Outgoing(Outgoing::Subscribe(pkid)) => PumpEvent::Subscribing(pkid),
                Event::Outgoing(Outgoing::Publish(pkid)) if counts.receipts => {
                    PumpEvent::Publishing { sequence: counts.written.load(Ordering::SeqCst), pkid }
                }
                Event::Incoming(Packet::PubAck(ack)) if counts.receipts => PumpEvent::PubAck {
                    pkid: ack.pkid,
                    result: if matches!(ack.reason, PubAckReason::Success | PubAckReason::NoMatchingSubscribers) {
                        Ok(())
                    } else {
                        Err(format!("broker refused snapshot PUBACK: {:?}", ack.reason))
                    },
                },
                Event::Incoming(Packet::SubAck(ack)) => PumpEvent::SubAck {
                    pkid: ack.pkid,
                    granted: ack.return_codes.iter().all(|code| matches!(code, SubscribeReasonCode::Success(_))),
                },
                Event::Outgoing(Outgoing::Unsubscribe(pkid)) => PumpEvent::Unsubscribing(pkid),
                Event::Incoming(Packet::UnsubAck(ack)) => PumpEvent::UnsubAck {
                    pkid: ack.pkid,
                    granted: ack.reasons.iter().all(|r| matches!(r, UnsubAckReason::Success | UnsubAckReason::NoSubscriptionExisted)),
                },
                Event::Outgoing(Outgoing::Disconnect) => {
                    // Read on until the broker closes the connection: closed with its acks still
                    // unread, the socket would be reset, and the broker would drop what it had not
                    // read yet — the last replies, Active=false, the DISCONNECT (confirming review
                    // N1).
                    if counts.disconnect_asked.load(Ordering::SeqCst) {
                        linger(&mut events, &counts.inflight).await;
                    }
                    PumpEvent::Disconnected
                }
                _ => continue,
            };
            let disconnected = matches!(forward, PumpEvent::Disconnected);
            let charge = match (&forward, &counts.buffers) {
                (PumpEvent::Publish(publish, _), Some(buffers)) => {
                    match buffers.reserve_incoming(crate::buffering::incoming_bytes(publish)) {
                        Some(charge) => Some(charge),
                        None => return end("the bounded incoming MQTT queue exhausted its byte capacity".into(), connected, false),
                    }
                }
                _ => None,
            };
            if tx.send(Queued { event: forward, _charge: charge }).is_err() || disconnected {
                // The session is gone, or asked to disconnect: polling on would reconnect.
                return;
            }
        }
    }
}

/// The pump's counts and signals, shared with the session's [`Incoming`]. Dropped with the pump's
/// task however it ends — returned, panicked or aborted, polled or not — it says the pump ended,
/// and wakes a wait on its progress.
struct Counts {
    buffers: Option<crate::buffering::Connection>,
    receipts: bool,
    inflight: Arc<AtomicUsize>,
    written: Arc<AtomicU64>,
    ids: Arc<AtomicU16>,
    max_packet: Arc<AtomicU32>,
    progress: Arc<tokio::sync::Notify>,
    disconnect_asked: Arc<AtomicBool>,
    accepted: Arc<AtomicBool>,
    ended: Arc<AtomicBool>,
}

impl Drop for Counts {
    fn drop(&mut self) {
        self.ended.store(true, Ordering::SeqCst);
        self.progress.notify_waiters();
    }
}

/// After the DISCONNECT, how long the pump reads on for the broker's close: within what the
/// session's flush keeps of its bound for the DISCONNECT and the close (third confirming reviews,
/// Claude minor 3: 1 s here and 1.5 s of ack wait overran the flush's 2 s).
pub(crate) const LINGER_BOUND: Duration = Duration::from_millis(500);

/// Poll the event loop after the DISCONNECT was written until the broker closes the connection —
/// every ack it sent read, so the close is orderly — or [`LINGER_BOUND`]: past it, one INFO
/// `bridge_shutdown_cut`. Never polled again after the close, which would reconnect.
async fn linger(events: &mut EventLoop, inflight: &AtomicUsize) {
    let deadline = Instant::now() + LINGER_BOUND;
    loop {
        // WAIT: mqtt-poll
        match tokio::time::timeout_at(deadline.into(), events.poll()).await {
            Ok(Ok(_)) => inflight.store(usize::from(events.state.inflight()), Ordering::SeqCst),
            Ok(Err(_)) => return,
            Err(_) => {
                info!(
                    kind = "bridge_shutdown_cut",
                    unacknowledged = events.state.inflight(),
                    bound_ms = LINGER_BOUND.as_millis() as u64,
                    "The broker did not close the connection after the DISCONNECT in time; closing it"
                );
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;

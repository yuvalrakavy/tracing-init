//! rumqttc's event loop, polled by a task that waits on nothing else (lifted from mqtt_ac's
//! `mqtt_pump.rs`; Store no-hang §14.3, §14.6).
//!
//! rumqttc's request channel drains only while the event loop is polled. The session waits on the
//! request channel (its publishes and subscribes), so a poller that did any of that work could wait
//! on itself. So the pump polls, and forwards what arrives on an unbounded queue — with a
//! high-water episode, since growth is a symptom to see — and the session does the rest.
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

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{Packet, Publish, SubscribeReasonCode};
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
    /// A publish, and when the pump received it: a request's expiry counts from then, not from
    /// when the session gets to it (review C7).
    Publish(Publish, Instant),
    /// A SUBSCRIBE was written, with its packet id: the session tells the read-back's SUBACK by it.
    Subscribing(u16),
    /// The broker answered a subscription: its packet id, and whether it granted every filter (an
    /// ACL may refuse one).
    SubAck { pkid: u16, granted: bool },
    /// The DISCONNECT the session asked for was written: the session's end.
    Disconnected,
    /// The connection failed or stalled, and its event loop is dropped: the session ends and the
    /// MQTT worker makes a fresh one.
    Ended(String),
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
    rx: UnboundedReceiver<PumpEvent>,
    backlog: Arc<Backlog>,
    /// The QoS 1 publishes the event loop has written and the broker not yet acknowledged.
    inflight: Arc<AtomicUsize>,
    /// The publishes the event loop has written in this session.
    written: Arc<AtomicU64>,
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

    /// The DISCONNECT the session is about to ask for is its own: the pump reads on after it until
    /// the broker closes, rather than for one rumqttc sends on a protocol error (the second
    /// confirming review's minor 3).
    pub fn expect_disconnect(&self) {
        self.disconnect_asked.store(true, Ordering::SeqCst);
    }

    pub async fn recv(&mut self) -> Option<PumpEvent> {
        // WAIT: mqtt-pump-queue
        let event = self.rx.recv().await;
        if matches!(event, Some(PumpEvent::Publish(..))) {
            self.backlog.popped();
        }
        event
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
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let backlog = Arc::new(Backlog::new());
        let inflight = Arc::new(AtomicUsize::new(0));
        let written = Arc::new(AtomicU64::new(0));
        let progress = Arc::new(tokio::sync::Notify::new());
        let disconnect_asked = Arc::new(AtomicBool::new(false));
        let accepted = Arc::new(AtomicBool::new(false));
        let ended = Arc::new(AtomicBool::new(false));
        let counts = Counts {
            inflight: inflight.clone(),
            written: written.clone(),
            progress: progress.clone(),
            disconnect_asked: disconnect_asked.clone(),
            accepted: accepted.clone(),
            ended: ended.clone(),
        };
        let task = tokio::spawn(Pump::run(events, tx, backlog.clone(), counts, outage, stall, on_lost).in_current_span());
        (Pump { task }, Incoming { rx, backlog, inflight, written, progress, disconnect_asked, accepted, ended })
    }

    async fn run(
        mut events: EventLoop,
        tx: UnboundedSender<PumpEvent>,
        backlog: Arc<Backlog>,
        counts: Counts,
        outage: Arc<BrokerOutage>,
        stall: Duration,
        on_lost: impl FnOnce() + Send + 'static,
    ) {
        let mut outstanding = Outstanding::default();
        let mut on_lost = Some(on_lost);
        let mut connected = false;
        let mut end = |reason: String, connected: bool| {
            if connected {
                if let Some(on_lost) = on_lost.take() {
                    on_lost();
                }
            }
            // Part of the outage's one episode; the event loop goes with this task, and the MQTT
            // worker makes a fresh client.
            outage.failed(&reason);
            debug!(error = %reason, "The MQTT connection ended; its session ends with it");
            let _ = tx.send(PumpEvent::Ended(reason));
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
                Ok(Err(e)) => return end(e.to_string(), connected),
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
                Event::Incoming(Packet::ConnAck(_)) => {
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
                Event::Incoming(Packet::SubAck(ack)) => PumpEvent::SubAck {
                    pkid: ack.pkid,
                    granted: ack.return_codes.iter().all(|code| matches!(code, SubscribeReasonCode::Success(_))),
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
            if tx.send(forward).is_err() || disconnected {
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
    inflight: Arc<AtomicUsize>,
    written: Arc<AtomicU64>,
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

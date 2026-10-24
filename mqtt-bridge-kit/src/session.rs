//! The MQTT side (lifted from mqtt_ac's `service.rs`, `mqtt_subscriber.rs` and
//! `mqtt_publisher.rs`; Store no-hang §14.3, §14.8–§14.9): one runtime for a fixed bridge's one
//! device and a multi-device host's many — a [`Bridge`](crate::Bridge) is the host's N = 1 mode
//! (multi-device spec §4.1).
//!
//! One session per connection, each with a client of its own: the pump polls in a task of its own
//! and forwards what arrives; the session reads it, posts requests to a device's mailbox — a post
//! never waits — and publishes what each device's [`Reports`] holds. It may wait on rumqttc's
//! request channel: back-pressure, which the pump's polling keeps moving, and which the pump bounds
//! (a broker that acknowledges nothing, or stops reading, ends the session). It never waits on a
//! device, so every CONNACK is read and answered whatever the devices are doing (no-hang 3b, review
//! B3).
//!
//! **The session part and the device part** (multi-device spec §4.1): `Shared` is the
//! session's — the broker, the bridge's own `Active` and `Version`, the outage episode, the
//! publication rotation's readiness signal; `Device` is one device's — its lanes, mailbox and
//! reports, and, for a host's configured instance, the `via` tag its `Status` and `State` carry.
//! Every session serves the host's lifecycle (the crate's private `lifecycle` module): a fixed
//! bridge's one instance, untagged, with R1's CONNACK duties, or a host's configured instances.
//!
//! **On every CONNACK** (the broker keeps no session) the lifecycle subscribes, then says
//! `Active=true` and the version, and the whole retained model goes out again from each device's
//! own state — a broker restarted without persistence lost the retained messages (the fleet's F4).
//! Nothing a previous connection queued goes out: its client went with it. A publish rumqttc
//! refuses is that one message dropped. Every SUBSCRIBE, UNSUBSCRIBE and publish is handed to
//! rumqttc within the lifecycle's packet-id budget (MQTT §2.2.1; multi-device spec §4.3).
//!
//! **The start-up read-back** (a fixed bridge): the process's first CONNACK subscribes to the
//! bridge's own retained `State` before anything else, and subscribes to its requests only once
//! that subscription is acknowledged (within [`READ_BACK_WAIT`]): the broker queues a
//! subscription's retained messages behind its SUBACK, so the retained `State` is on the one
//! stream ahead of every retained request (review C9). It is kept as the outage conflict rule's
//! "before" values after a restart (Store kit spec §4.5). Only retained deliveries count: the
//! bridge's own later publishes come back unflagged, and are ignored. A session that ends before
//! the SUBACK leaves the read-back to the next. (A configured instance's read-back is its S1.)
//!
//! **Publication and fairness** (multi-device spec §4.7): each device's output is R1's
//! cancellation-safe `Batch`, *sealed* when taken — later State goes to the next batch — so a
//! reply follows the State its command produced and is never starved by later State. Devices are
//! served round-robin with a persistent cursor, at most [`QUANTUM`] messages per device per turn,
//! then the ready inbound and lifecycle work, then the next device.
//!
//! **Shutdown:** what waits refused, every device's answers first, then the rest; `Active=false`,
//! then DISCONNECT, polling until it is written — within [`FLUSH_BOUND`]; past it the last will says
//! `Active=false` instead (INFO `bridge_shutdown_cut`). The flush publishes within the packet-id
//! budget while a SUBSCRIBE or UNSUBSCRIBE is outstanding, and past its ack wait publishes nothing
//! more: no `Active=false`, no DISCONNECT — the last will says it (multi-device spec §4.3).

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lock_order::sync::Mutex;
use rumqttc::v5::mqttbytes::v5::{LastWill, Publish, PublishProperties};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, MqttOptions};
use tokio::sync::{mpsc, watch, Notify};
use tracing::{debug, info, warn, Instrument, Span};

use crate::devices::Event;
use crate::driver::{CommandClass, CommandSpec};
use crate::lifecycle::Lifecycle;
use crate::mailbox::{Check, Mailbox, Posted, ReadKey, StateValue};
use crate::pump::{stall_bound, BrokerOutage, Incoming, Pump, PumpEvent, MAX_CONSECUTIVE_ERRORS};
use crate::replies::{reply_payload, Answer, ReplyTo};
use crate::reports::{Changes, Model, Outbound, Reports, Sealed};
use crate::topics::{Inbound, Topics};
use crate::wire::{self, ErrorReport, Reason};

/// The bound on the shutdown's flush: `Active=false` and DISCONNECT, written.
pub const FLUSH_BOUND: Duration = Duration::from_secs(2);

/// What the flush keeps of its bound, past the ack wait, for the DISCONNECT's write and the
/// pump's linger after it, until the broker closes ([`crate::pump::LINGER_BOUND`], 500 ms): the
/// ack wait ends this long before the bound (third confirming reviews, Claude minor 3).
const DISCONNECT_PART: Duration = Duration::from_millis(750);

// The linger and the DISCONNECT's write fit in their part, with 250 ms for the write.
const _: () = assert!(crate::pump::LINGER_BOUND.as_millis() + 250 <= DISCONNECT_PART.as_millis());

/// How long a fixed bridge's first CONNACK waits for the read-back's SUBACK: past it, the read-back
/// is incomplete (WARN `read_back_incomplete`) and the session ends — only a new connection frees
/// the abandoned SUBSCRIBE's packet id — and the next connection subscribes to the requests without
/// it.
pub const READ_BACK_WAIT: Duration = Duration::from_secs(5);

/// The largest packet the bridge takes, advertised in its CONNECT, so a broker discards a larger
/// message for it rather than send it. rumqttc's default, 10 KiB, is enforced but not advertised:
/// the broker replayed a larger retained `Config` or `Desired` on every subscribe and rumqttc
/// dropped the connection each time — a reconnect loop (review B3). 1 MiB holds the largest
/// payloads the fleet expects (IR code sets, DMX arrays and effects: tens of KiB) many times over,
/// matches the stdio plugin protocol's line cap (spec §7), and bounds what one message can cost.
pub const MAX_PACKET_SIZE: u32 = 1024 * 1024;

/// At most this many messages of one device per turn of the publication rotation (multi-device
/// spec §4.7); then the ready inbound and lifecycle work, then the next device.
pub const QUANTUM: usize = 32;

/// The test seam's environment variable (with the `test-seams` feature only): see `panic_for_test`.
pub const TEST_PANIC_TOPIC: &str = "MQTT_BRIDGE_TEST_PANIC_TOPIC";

/// The MQTT side's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqttTiming {
    /// MQTT keep-alive (at least 5 s, rumqttc's floor). Twice it bounds how long the broker may
    /// leave a request unacknowledged, and one poll may take ([`crate::pump::stall_bound`]).
    pub keep_alive: Duration,
    /// After `MAX_CONSECUTIVE_ERRORS` failed connects in a row (five), the pause before the next.
    pub retry: Duration,
    /// The pause before a reconnect otherwise. Each connection is a session of its own, with a
    /// fresh client.
    pub poll_retry: Duration,
    /// A broker outage this long is one WARN (`external_failure`).
    pub warn_after: Duration,
    /// A connection made during an outage that lasts this long ends it.
    pub stable_after: Duration,
}

impl Default for MqttTiming {
    fn default() -> MqttTiming {
        MqttTiming {
            keep_alive: Duration::from_secs(5),
            retry: Duration::from_secs(10),
            poll_retry: Duration::from_secs(1),
            warn_after: Duration::from_secs(30),
            stable_after: Duration::from_secs(5),
        }
    }
}

/// The session part (multi-device spec §4.1): what every session shares, for the life of the
/// bridge or host.
pub(crate) struct Shared {
    /// The process's own lanes — `Active/{bridge id}` (and its last will) and `Version/{bridge
    /// id}` — and its MQTT client id. A fixed bridge's bridge id is its instance.
    pub topics: Topics,
    pub broker_host: String,
    pub broker_port: u16,
    pub timing: MqttTiming,
    /// The text of the retained `Version`.
    pub version: String,
    pub outage: Arc<BrokerOutage>,
    /// The QoS 1 publishes this session has enqueued; the pump counts those it wrote.
    pub enqueued: std::sync::atomic::AtomicU64,
    /// Told by every device's reports: one wait serves them all.
    pub ready: Arc<Notify>,
    /// Every device's reports, by device id, for the broker-loss snapshot the pump takes at once.
    pub devices: Mutex<Vec<(u64, Arc<Reports>)>>,
}

impl Shared {
    /// The MQTT connection was lost: every device's model as it stands is what it knew when it
    /// stopped listening (review B1). Never waits; called by the pump.
    fn broker_lost(&self) {
        let devices: Vec<Arc<Reports>> = {
            // WAIT: session-devices
            let devices = self.devices.lock().unwrap_or_else(|p| p.into_inner());
            devices.iter().map(|(_, reports)| reports.clone()).collect()
        };
        for reports in devices {
            reports.broker_lost();
        }
    }
}

/// The device part (multi-device spec §4.1): one device's lanes, commands, mailbox and reports.
pub(crate) struct Device {
    pub topics: Topics,
    /// The commands its driver declares, once its driver is known (a host's device runtime may
    /// receive requests before its construction is done).
    pub commands: std::sync::OnceLock<Vec<CommandSpec>>,
    pub mailbox: Arc<Mailbox>,
    pub reports: Arc<Reports>,
    /// A configured instance's: every `Status` and `State` it publishes carries the MQTT v5 user
    /// property `via` = this bridge id (multi-device spec §4.5). A fixed device: `None`, never
    /// tagged.
    pub tag: Option<String>,
    /// The device's span: its instance (and the host's bridge id, from its parent).
    pub span: Span,
}

/// After a turn of the rotation, how many ready incoming events go before the next turn.
const READS_PER_BATCH: u32 = 64;

/// How a session ended.
enum SessionEnd {
    /// Its connection failed, or never came (`connected`: when the broker accepted it, if it did);
    /// a new session follows.
    Ended { connected: Option<Instant> },
    /// The shutdown.
    Shutdown,
}

/// What a session's handler says: go on, or end the session (a subscription refused or overdue).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flow {
    Continue,
    End,
}

/// What a handler acts on: the session part, this connection's client, and its pump's publish
/// counts.
pub(crate) struct Ctx<'a> {
    pub shared: &'a Shared,
    pub client: &'a AsyncClient,
    pub line: crate::pump::Line,
}

impl Ctx<'_> {
    /// Every publish this session enqueued is written and acknowledged: a SUBSCRIBE or UNSUBSCRIBE
    /// sent now takes a packet id nothing else holds (MQTT §2.2.1).
    pub(crate) fn line_clear(&self) -> bool {
        self.line.clear(self.shared.enqueued.load(Ordering::SeqCst))
    }
}

/// One device's place in the publication rotation.
pub(crate) struct Slot {
    pub id: u64,
    pub device: Arc<Device>,
    /// What was taken from its reports and is not published yet: sealed until it is.
    pub batch: Batch,
    /// Its output may go out on this connection: the fixed instance's `Active=true` is said, or a
    /// configured instance's read-back is granted (published sooner, a State would overwrite the
    /// retained one its read-back is there to read) — or it is stopping, when its read-back no
    /// longer matters.
    pub announced: bool,
}

/// The publication rotation (multi-device spec §4.7): every device's sealed batch, served
/// round-robin with a persistent cursor, at most [`QUANTUM`] messages per turn; and the outbox, the
/// messages of no device (a host's instance-level `Error`s, its cleanup's retractions).
pub(crate) struct Outputs {
    pub slots: Vec<Slot>,
    cursor: usize,
    pub outbox: Batch,
    /// The outbox may go out: the connection's root subscriptions are made.
    pub outbox_open: bool,
    /// A turn may find more without a new report: the last one found something, or the session is
    /// new.
    owed: bool,
    /// How many more messages may be published now; `None`, no limit. A host's SUBSCRIBE or
    /// UNSUBSCRIBE outstanding allows only as many as rumqttc can hand out before its packet id
    /// comes round again; one waiting for the line to clear of publishes, none (MQTT §2.2.1; R2
    /// gate, Codex 3).
    budget: Option<usize>,
}

impl Outputs {
    pub(crate) fn new() -> Outputs {
        Outputs { slots: Vec::new(), cursor: 0, outbox: Batch::default(), outbox_open: false, owed: true, budget: None }
    }

    /// Set the publication's budget (a turn owed when it grows: what waited may go).
    pub(crate) fn set_budget(&mut self, budget: Option<usize>) {
        let grows = match (self.budget, budget) {
            (Some(before), Some(now)) => now > before,
            (Some(_), None) => true,
            (None, _) => false,
        };
        if grows {
            self.owed = true;
        }
        self.budget = budget;
    }

    /// A device joins the rotation (and the broker-loss snapshot).
    pub(crate) fn add(&mut self, shared: &Shared, id: u64, device: Arc<Device>) {
        {
            // WAIT: session-devices
            let mut devices = shared.devices.lock().unwrap_or_else(|p| p.into_inner());
            devices.push((id, device.reports.clone()));
        }
        self.slots.push(Slot { id, device, batch: Batch::default(), announced: false });
        self.owed = true;
    }

    /// A device leaves the rotation; what its batch held goes with it.
    pub(crate) fn remove(&mut self, shared: &Shared, id: u64) -> Option<Slot> {
        {
            // WAIT: session-devices
            let mut devices = shared.devices.lock().unwrap_or_else(|p| p.into_inner());
            devices.retain(|(d, _)| *d != id);
        }
        let i = self.slots.iter().position(|s| s.id == id)?;
        let slot = self.slots.remove(i);
        if self.cursor > i {
            self.cursor -= 1;
        }
        // Positions 0..n are the devices, n the outbox.
        if self.cursor > self.slots.len() {
            self.cursor = 0;
        }
        Some(slot)
    }

    /// A message of no device, queued in the outbox — published at the next turn.
    pub(crate) fn queue_outbox(&mut self, part: Part, out: Out) {
        self.outbox.push(part, out);
        self.owed = true;
    }

    /// Device `id`'s slot.
    pub(crate) fn slot(&mut self, id: u64) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.id == id)
    }

    /// Device `id`'s output may go out on this connection: announced, and a turn owed — what its
    /// reports hold may have rung the readiness signal while nothing could be published. Whether
    /// the device is in the rotation.
    pub(crate) fn announce(&mut self, id: u64) -> bool {
        match self.slot(id) {
            Some(slot) => {
                slot.announced = true;
                self.owed = true;
                true
            }
            None => false,
        }
    }

    /// Whether a message to be said before `Active=true` still waits — in a device's batch, or in
    /// the outbox (multi-device spec §4.6).
    pub(crate) fn firsts_waiting(&self) -> bool {
        self.outbox.has_firsts() || self.slots.iter().any(|s| s.batch.has_firsts())
    }

    /// A new connection: nothing is announced on it yet, and nothing a previous connection's batch
    /// held goes out (its client went with it; the model is republished).
    fn session_started(&mut self) {
        for slot in &mut self.slots {
            slot.announced = false;
            slot.batch = Batch::default();
        }
        // A host's outbox not yet published — instance-level Errors, terminal answers, a retired
        // runtime's answers, cleanups (pending again in the lifecycle until acknowledged) — is lost
        // with its connection, and said (multi-device spec §4.4 step 3; R2 gate, Claude minor 2).
        if !self.outbox.is_empty() {
            warn!(
                kind = "output_lost_at_session_end",
                lost = self.outbox.len(),
                "Messages of no device were not published before the session ended; dropped"
            );
        }
        self.outbox = Batch::default();
        self.outbox_open = false;
        self.owed = true;
        self.budget = None;
    }

    /// Whether anything may be published now.
    fn open(&self) -> bool {
        self.budget != Some(0) && (self.slots.iter().any(|s| s.announced) || (self.outbox_open && !self.outbox.is_empty()))
    }

    /// One turn of the rotation: the next member after the cursor with output — a device's sealed
    /// batch, or a new one taken from its reports when it is empty; or the outbox, the member after
    /// the devices — at most [`QUANTUM`] messages. Returns the device served, if any. Cancellation-safe: what is
    /// taken stays in the slot's batch.
    async fn turn(&mut self, shared: &Shared, client: &AsyncClient) -> Option<u64> {
        let limit = self.budget.map_or(QUANTUM, |budget| budget.min(QUANTUM));
        if limit == 0 {
            return None;
        }
        // The outbox is one member of the rotation, after the devices (position `n`): served in
        // its turn, a quantum at a time, so neither a steady outbox — terminal answers to a flood
        // of late requests — nor a steady device starves the other (R2 gate, Codex 2).
        let n = self.slots.len();
        for k in 0..=n {
            let i = (self.cursor + k) % (n + 1);
            if i == n {
                if self.outbox_open && !self.outbox.is_empty() {
                    // WAIT: mqtt-request
                    self.outbox.publish(shared, client, &IN_TURN, limit).await;
                    self.cursor = 0;
                    self.owed = true;
                    return None;
                }
                continue;
            }
            let slot = &mut self.slots[i];
            if !slot.announced {
                continue;
            }
            // Sealed: a new batch only once the last one is published.
            if slot.batch.is_empty() {
                slot.batch.add(&slot.device, slot.device.reports.take_sealed());
            }
            if slot.batch.is_empty() {
                continue;
            }
            let id = slot.id;
            // WAIT: mqtt-request
            slot.batch.publish(shared, client, &IN_TURN, limit).await;
            self.cursor = (i + 1) % (n + 1);
            self.owed = true;
            return Some(id);
        }
        self.owed = false;
        None
    }
}

/// The MQTT side, for the life of the bridge: sessions one after another, one per connection,
/// until `shutdown` turns true. The pause before the next is `poll_retry`, and `retry` after every
/// `MAX_CONSECUTIVE_ERRORS`th failed connect in a row. The lifecycle's events go on between
/// sessions.
pub(crate) async fn run(
    shared: Arc<Shared>,
    mut host: Lifecycle,
    mut outputs: Outputs,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut failed: u32 = 0;
    loop {
        let connected = match session(&shared, &mut host, &mut outputs, &mut events, &mut shutdown).await {
            SessionEnd::Shutdown => return,
            SessionEnd::Ended { connected } => connected,
        };
        // A connection lost after it held is the first failure of what follows; a connect refused,
        // or a connection dropped before `stable_after` (two bridges sharing a client id), one more
        // (Claude's re-gate minor 12: every accepted connection reset the count).
        // Its age counts from the CONNACK: a slow one is no time the connection held (re-gate 4
        // minor 9).
        failed = if connected.is_some_and(|at| at.elapsed() >= shared.timing.stable_after) { 1 } else { failed + 1 };
        let pause = if failed % MAX_CONSECUTIVE_ERRORS == 0 { shared.timing.retry } else { shared.timing.poll_retry };
        debug!(pause_ms = pause.as_millis() as u64, failed, "MQTT session ended; starting a new one");
        let resume = tokio::time::Instant::now() + pause;
        loop {
            // WAIT: session-retry
            let event = tokio::select! {
                () = tokio::time::sleep_until(resume) => break,
                _ = shutdown.wait_for(|stop| *stop) => return,
                Some(event) = events.recv() => event,
            };
            host.on_event_offline(&mut outputs, event).await;
        }
    }
}

fn client(shared: &Shared) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let topics = &shared.topics;
    let client_id = format!("{}-{}", topics.root(), topics.instance());
    let mut options = MqttOptions::new(client_id, shared.broker_host.clone(), shared.broker_port);
    let will = LastWill::new(topics.active(), "false", QoS::AtLeastOnce, true, None);
    options.set_keep_alive(shared.timing.keep_alive).set_clean_start(true).set_last_will(will).set_max_packet_size(Some(MAX_PACKET_SIZE));
    AsyncClient::new(options, 100)
}

/// Outbound publishes carry the current trace's `traceparent`, if a trace is active.
pub(crate) fn properties() -> PublishProperties {
    let mut properties = PublishProperties::default();
    if let Some(traceparent) = tracing_init::traceparent::current() {
        properties.user_properties.push(("traceparent".into(), traceparent));
    }
    properties
}

/// [`properties`], with a configured instance's `via` tag when it has one (multi-device spec §4.5).
pub(crate) fn tagged(tag: Option<&str>) -> PublishProperties {
    let mut properties = properties();
    if let Some(tag) = tag {
        properties.user_properties.push(("via".into(), tag.to_owned()));
    }
    properties
}

/// A request rumqttc refused (it carries the refused request): the session ends on it. Boxed, as
/// it passes up every handler's `Result`, and the bare `ClientError` is over 200 bytes
/// (`clippy::result_large_err`).
pub(crate) type Failed = Box<rumqttc::v5::ClientError>;

/// Publish one message. A refusal — a topic rumqttc does not publish to, or the event loop gone —
/// is that one message dropped, said at DEBUG; the caller goes on with the rest (review B2: the
/// first refusal ended the CONNACK duties, and the session never published a report again).
pub(crate) async fn emit(
    shared: &Shared,
    client: &AsyncClient,
    topic: String,
    retain: bool,
    payload: Vec<u8>,
    properties: PublishProperties,
) {
    let refused_topic = topic.clone();
    // WAIT: mqtt-request
    let sent = client.publish_with_properties(topic, QoS::AtLeastOnce, retain, payload, properties).await;
    let _ = enqueued(shared, &refused_topic, sent);
}

/// What came of a publish: counted when rumqttc took it — the shutdown waits until the pump has
/// written, and the broker acknowledged, every publish enqueued (the second confirming review's
/// minor) — else that one message dropped, said at DEBUG.
fn enqueued(shared: &Shared, topic: &str, sent: Result<(), rumqttc::v5::ClientError>) -> Option<u64> {
    match sent {
        Ok(()) => Some(shared.enqueued.fetch_add(1, Ordering::SeqCst) + 1),
        Err(e) => {
            debug!(topic, error = %e, "An MQTT publish was refused; this message is dropped");
            None
        }
    }
}

/// One session: a fresh client and its pump, for one connection, until it ends or the shutdown.
async fn session(
    shared: &Arc<Shared>,
    host: &mut Lifecycle,
    outputs: &mut Outputs,
    events: &mut mpsc::UnboundedReceiver<Event>,
    shutdown: &mut watch::Receiver<bool>,
) -> SessionEnd {
    let (client, pump_events) = client(shared);
    shared.enqueued.store(0, Ordering::SeqCst);
    // Polling first, so every request below has an event loop draining it.
    // When the connection is lost, what the bridge knew then is the conflict rule's "before" values
    // for what the broker replays on the next one (review B1).
    let lost = shared.clone();
    let (_pump, mut incoming) =
        Pump::start(pump_events, shared.outage.clone(), stall_bound(shared.timing.keep_alive), move || lost.broker_lost());
    // Nothing is published before the CONNACK's duties allow it: the subscriptions come first,
    // and a state published ahead of the read-back's subscription would overwrite the retained
    // state it is there to read.
    for slot in &outputs.slots {
        slot.device.reports.removals_connection_started();
    }
    outputs.session_started();
    // When the broker accepted this session's connection, if it did.
    let mut accepted: Option<Instant> = None;
    // How many ready incoming events still go before the next turn of the rotation.
    let mut reads_owed: u32 = 0;
    let ctx = Ctx { shared, client: &client, line: incoming.line() };
    loop {
        let wake = host.next_wake();
        let open = outputs.open();
        let owed = open && outputs.owed;
        // A SUBSCRIBE or UNSUBSCRIBE waiting for the line to clear of publishes: the pump's next
        // event may clear it (enabled before the check, so no clearing is missed).
        if incoming.connected() {
            let acknowledged = ctx.line.acknowledged();
            for slot in &outputs.slots {
                slot.device.reports.retire_acknowledged_removals(acknowledged);
            }
        }
        let line_wait = host.line_wait() || outputs.slots.iter().any(|slot| slot.device.reports.removals_inflight());
        let mut progress = Box::pin(incoming.progress());
        progress.as_mut().enable();
        let line_cleared = host.line_wait() && ctx.line_clear();
        // The stop first: its flush publishes what is pending anyway (re-gate R15). Then, after a
        // turn of the rotation, the incoming events that are ready — up to READS_PER_BATCH — and
        // the lifecycle's, before the next turn: a steady stream of reports to a broker slow to
        // acknowledge keeps the rotation ready at every pass, and the requests, commands and
        // SUBACKs behind it would wait for good (Codex final gate 1); one read per batch drained a
        // backlog — the bridge's own State coming back before the read-back's subscription was
        // dropped — at a read a second, and a command behind it waited past its expiry (the second
        // confirming review's minor 5). With no read owed, what is pending goes first: answers are
        // uncapped, so no backlog drained first can displace them (re-gate 3 S2).
        // A timer due — an overdue grant, a failed instance's retry, the sweep — and the lifecycle's
        // events — a construction done, a worker stopped — get their service whatever the inbound
        // traffic and the publication do (R2 gate, Codex 1: a steady stream of reports kept the
        // rotation owed at every pass, and an overdue SUBACK never ended the session; a steady
        // inbound stream could strand a construction's end). The timer comes right after the stop,
        // so a due one goes first — through tokio's timer, which keeps the task cooperative should
        // a handler ever leave a timer due — and each handler clears what it served; the events are
        // finite, made only by the lifecycle's own work, so ahead of the inbound they cannot starve
        // it.
        let step = if reads_owed > 0 {
            // WAIT: session-events
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => Step::Shutdown,
                () = sleep_until(wake) => Step::Tick,
                () = std::future::ready(()), if line_cleared => Step::Line,
                Some(event) = events.recv() => Step::Lifecycle(event),
                event = incoming.recv() => Step::Event(event),
                () = std::future::ready(()), if owed => Step::Reported,
                () = shared.ready.notified(), if open => Step::Reported,
                () = &mut progress, if line_wait => Step::Line,
            }
        } else {
            // WAIT: session-events
            tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => Step::Shutdown,
                () = sleep_until(wake) => Step::Tick,
                () = std::future::ready(()), if line_cleared => Step::Line,
                () = std::future::ready(()), if owed => Step::Reported,
                () = shared.ready.notified(), if open => Step::Reported,
                Some(event) = events.recv() => Step::Lifecycle(event),
                event = incoming.recv() => Step::Event(event),
                () = &mut progress, if line_wait => Step::Line,
            }
        };
        drop(progress);
        reads_owed = match step {
            Step::Reported => READS_PER_BATCH,
            _ => reads_owed.saturating_sub(1),
        };

        let connack = matches!(step, Step::Event(Some(PumpEvent::Connected)));
        if connack && accepted.is_none() {
            accepted = Some(Instant::now());
        }
        let work = async {
            match step {
                Step::Event(Some(PumpEvent::Connected)) => host.on_connack(&ctx, outputs).await.map(|()| None),
                Step::Event(Some(PumpEvent::Publish(publish, received))) => {
                    let (topic, span) = traced(&publish, shared.topics.instance());
                    let flow = host.on_publish(&ctx, outputs, &topic, &publish, received).instrument(span).await;
                    flow.map(ended)
                }
                Step::Event(Some(PumpEvent::Disconnected)) => Ok(None),
                Step::Event(Some(
                    event @ (PumpEvent::SubAck { .. }
                    | PumpEvent::Subscribing(_)
                    | PumpEvent::Unsubscribing(_)
                    | PumpEvent::UnsubAck { .. }),
                )) => host.on_ack(&ctx, outputs, event).await.map(ended),
                Step::Event(Some(PumpEvent::Ended(_))) => Ok(Some(SessionEnd::Ended { connected: None })),
                // The pump went without a word — it panicked — so it never said the connection was
                // lost: the broker snapshot is taken here (Claude's re-gate minor 14).
                Step::Event(None) => {
                    if accepted.is_some() {
                        shared.broker_lost();
                    }
                    Ok(Some(SessionEnd::Ended { connected: None }))
                }
                Step::Lifecycle(event) => host.on_event(&ctx, outputs, event).await.map(ended),
                Step::Tick => host.on_timer(&ctx, outputs).await.map(ended),
                Step::Line => host.on_line(&ctx, outputs).await.map(ended),
                Step::Reported => {
                    outputs.turn(shared, &client).await;
                    host.on_turn_done(&ctx, outputs).await.map(ended)
                }
                Step::Shutdown => Ok(Some(SessionEnd::Shutdown)),
            }
        };
        // A publish waits while the broker stalls; the shutdown does not wait behind it.
        // WAIT: session-work
        let done = tokio::select! {
            done = work => done,
            _ = shutdown.wait_for(|stop| *stop) => Ok(Some(SessionEnd::Shutdown)),
        };
        match done {
            Ok(None) => {}
            Ok(Some(SessionEnd::Ended { .. })) => {
                host.on_session_lost(outputs).await;
                return SessionEnd::Ended { connected: accepted };
            }
            Ok(Some(SessionEnd::Shutdown)) => {
                flush(shared, &client, &mut incoming, outputs, host).await;
                return SessionEnd::Shutdown;
            }
            // The event loop is gone (the pump ended): the next event says so.
            Err(e) => debug!(error = %e, "An MQTT request failed: the event loop is gone"),
        }
    }
}

/// A handler's flow, as the session's step.
fn ended(flow: Flow) -> Option<SessionEnd> {
    match flow {
        Flow::Continue => None,
        Flow::End => Some(SessionEnd::Ended { connected: None }),
    }
}

/// Sleep until `at`, or for good when there is none.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        // WAIT: session-events
        None => std::future::pending::<()>().await,
    }
}

// Transient: one per pass of the session's loop, never stored.
#[allow(clippy::large_enum_variant)]
enum Step {
    Event(Option<PumpEvent>),
    Lifecycle(Event),
    Reported,
    Tick,
    /// The line has cleared of publishes for a waiting SUBSCRIBE or UNSUBSCRIBE.
    Line,
    Shutdown,
}

/// One message of a batch, ready to publish.
pub(crate) struct Out {
    topic: String,
    retain: bool,
    payload: Vec<u8>,
    properties: PublishProperties,
    /// A host's: said before `Active=true` on this connection (multi-device spec §4.6) — the
    /// session's `Active` waits until no such message waits ([`Outputs::firsts_waiting`]).
    first: bool,
    cleanup: Option<(Arc<Reports>, String, u64)>,
}

impl Out {
    #[cfg(test)]
    pub(crate) fn traceparent_for_test(&self) -> Option<&str> {
        self.properties.user_properties.iter().find(|(key, _)| key == "traceparent").map(|(_, value)| value.as_str())
    }
    /// A message; `tag`, a configured instance's `via` for its `Status` and `State`.
    pub(crate) fn new(topic: String, retain: bool, payload: Vec<u8>, correlation: Option<bytes::Bytes>, tag: Option<&str>) -> Out {
        let mut properties = tagged(tag);
        properties.correlation_data = correlation;
        Out { topic, retain, payload, properties, first: false, cleanup: None }
    }

    /// The same, said before `Active=true` (multi-device spec §4.6).
    pub(crate) fn first(self) -> Out {
        Out { first: true, ..self }
    }

    /// Stamp the sealed observation/request context, never the current table.
    pub(crate) fn context(mut self, context: &crate::RequestContext) -> Self {
        if context.trace_was_captured() {
            self.properties.user_properties.retain(|(key, _)| key != "traceparent");
            if let Some(parent) = context.traceparent() {
                self.properties.user_properties.push(("traceparent".into(), parent.into()));
            }
        }
        if let Some(generation) = context.generation() {
            self.properties.user_properties.push(("target-generation".into(), generation.to_owned()));
        }
        self
    }

    /// Whether it is an empty retained message — a retraction — on `topic`, or under `prefix`.
    pub(crate) fn retracts(&self, topic: &str, prefix: &str) -> bool {
        self.retain && self.payload.is_empty() && (self.topic == topic || self.topic.starts_with(prefix))
    }
}

/// A publish under way: its topic, for the refusal's line, and rumqttc's request.
type CleanupReceipt = (Arc<Reports>, String, u64);
type Sending =
    std::pin::Pin<Box<dyn std::future::Future<Output = (String, Result<(), rumqttc::v5::ClientError>, Option<CleanupReceipt>)> + Send>>;

/// A part of a batch, in the order a publication takes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Part {
    /// Every terminal `Error` of a request, then every reply.
    Answers,
    /// `Status`, then each changed `State`.
    Model,
    /// The capped `Error`s and `Event`s.
    Unsolicited,
}

/// In normal operation a reply follows the State its command produced — a `refresh`'s reply says
/// its State is current — so Status and State go first, then the answers (the second confirming
/// review's F1); the unsolicited reports last.
pub(crate) const IN_TURN: [Part; 3] = [Part::Model, Part::Answers, Part::Unsolicited];

/// What one device reported, taken from its reports, and how far its publication has got — the
/// session's, outside the work a stop cuts short: a stop in the middle of a batch hands the flush
/// what the batch has not enqueued, its answers first, and nothing enqueued goes twice (third
/// confirming reviews, Codex 1: the batch was taken out of the reports whole, and the stop dropped
/// what it had not published — a `refresh`'s reply among it, owned by nobody).
#[derive(Default)]
pub(crate) struct Batch {
    /// Every terminal `Error` of a request, then every reply.
    answers: VecDeque<Out>,
    /// `Status`, then each changed `State`.
    model: VecDeque<Out>,
    /// The capped `Error`s and `Event`s.
    unsolicited: VecDeque<Out>,
    /// The publish under way. A stop that cuts it short leaves it here, not dropped — whether
    /// rumqttc's channel took the message cannot be told from outside — and the next publication
    /// finishes it first.
    sending: Option<Sending>,
    /// How many of the messages waiting, the one under way included, are to be said before
    /// `Active=true` (`Out::first`).
    firsts: usize,
    /// The publish under way is one of them.
    sending_first: bool,
}

impl Batch {
    /// Add what a device's reports gave, behind what is still waiting.
    pub(crate) fn add(&mut self, device: &Device, changes: Sealed<Changes>) {
        let topics = &device.topics;
        let tag = device.tag.as_deref();
        let Sealed { snapshot: Changes { answer_errors, replies, status, states, errors, events }, retractions } = changes;
        self.answers.extend(answer_errors.into_iter().map(|e| Out::new(topics.error(), false, e.encode(), None, None).context(&e.context)));
        self.answers.extend(
            replies
                .into_iter()
                .map(|r| Out::new(r.topic.unwrap_or_else(|| topics.event()), false, r.payload, r.correlation, None).context(&r.context)),
        );
        self.model.extend(status.map(|s| Out::new(topics.status(), true, s.as_str().as_bytes().to_vec(), None, tag)));
        self.model.extend(states.into_iter().map(|(target, document, context)| {
            let mut out = Out::new(topics.state(&target), true, document, None, tag).context(&context);
            out.cleanup = retractions.get(&target).map(|nonce| (device.reports.clone(), target, *nonce));
            out
        }));
        self.unsolicited.extend(errors.into_iter().map(|e| Out::new(topics.error(), false, e.encode(), None, None).context(&e.context)));
        self.unsolicited.extend(
            events
                .into_iter()
                .map(|e| Out::new(e.topic.unwrap_or_else(|| topics.event()), false, e.payload, e.correlation, None).context(&e.context)),
        );
    }

    /// Queue `out` in `part`, behind what waits there.
    pub(crate) fn push(&mut self, part: Part, out: Out) {
        self.firsts += usize::from(out.first);
        self.queue(part).push_back(out);
    }

    /// A device's whole retained model — its `Status`, then every target's `State` — behind what
    /// waits, published through the rotation within the packet-id budget (multi-device spec §4.3;
    /// the R2 confirming reviews: the model went out by direct publishes, outside the budget).
    /// `first`: said before `Active=true` — a configured instance's, once its read-back is granted
    /// (§4.6); the fixed instance's goes after it, as R1's did.
    pub(crate) fn add_model(&mut self, device: &Device, model: Sealed<Model>, first: bool) {
        let Sealed { snapshot: model, retractions } = model;
        let (topics, tag) = (&device.topics, device.tag.as_deref());
        debug!(targets = model.states.len(), "Publishing the retained model again after a CONNACK");
        let said = |out: Out| if first { out.first() } else { out };
        if let Some(status) = model.status {
            self.push(Part::Model, said(Out::new(topics.status(), true, status.as_str().as_bytes().to_vec(), None, tag)));
        }
        for (target, document, context) in model.states {
            let mut out = Out::new(topics.state(&target), true, document, None, tag).context(&context);
            out.cleanup = retractions.get(&target).map(|nonce| (device.reports.clone(), target, *nonce));
            self.push(Part::Model, said(out));
        }
    }

    /// Drop the waiting messages of `part` that `pick` picks; how many.
    pub(crate) fn discard(&mut self, part: Part, pick: impl Fn(&Out) -> bool) -> usize {
        let (dropped, kept): (VecDeque<Out>, VecDeque<Out>) = std::mem::take(self.queue(part)).into_iter().partition(pick);
        self.firsts -= dropped.iter().filter(|out| out.first).count();
        *self.queue(part) = kept;
        dropped.len()
    }

    /// Whether a message to be said before `Active=true` waits.
    pub(crate) fn has_firsts(&self) -> bool {
        self.firsts > 0
    }

    fn queue(&mut self, part: Part) -> &mut VecDeque<Out> {
        match part {
            Part::Answers => &mut self.answers,
            Part::Model => &mut self.model,
            Part::Unsolicited => &mut self.unsolicited,
        }
    }

    /// Every message of `part` waiting, taken out.
    pub(crate) fn drain(&mut self, part: Part) -> Vec<Out> {
        let drained: Vec<Out> = self.queue(part).drain(..).collect();
        self.firsts -= drained.iter().filter(|out| out.first).count();
        drained
    }

    /// Whether nothing waits: no message, and no publish under way.
    pub(crate) fn is_empty(&self) -> bool {
        self.sending.is_none() && self.answers.is_empty() && self.model.is_empty() && self.unsolicited.is_empty()
    }

    /// How many messages wait, the one under way included.
    pub(crate) fn len(&self) -> usize {
        usize::from(self.sending.is_some()) + self.answers.len() + self.model.len() + self.unsolicited.len()
    }

    /// Publish what waits, the publish under way first, then `parts` in their order, until `limit`
    /// messages have been handed over. At the stop, answers go first: the flush is bounded, and they
    /// are what must not be cut (confirming review 3). A message refused is dropped alone.
    /// Cancellation-safe: a message leaves the batch only into `sending`, which is kept. Returns how
    /// many were handed over.
    pub(crate) async fn publish(&mut self, shared: &Shared, client: &AsyncClient, parts: &[Part], limit: usize) -> usize {
        let mut handed = 0;
        // No packet id to spare: not even the publish under way is finished.
        if limit == 0 {
            return handed;
        }
        loop {
            if let Some(sending) = self.sending.as_mut() {
                // WAIT: mqtt-request
                let (topic, sent, cleanup) = sending.await;
                self.sending = None;
                if std::mem::take(&mut self.sending_first) {
                    self.firsts -= 1;
                }
                if let (Some(sequence), Some((reports, target, nonce))) = (enqueued(shared, &topic, sent), cleanup) {
                    reports.removal_sent(&target, nonce, sequence);
                }
                handed += 1;
            }
            if handed >= limit {
                return handed;
            }
            let mut next = None;
            for part in parts {
                if let Some(out) = self.queue(*part).pop_front() {
                    next = Some(out);
                    break;
                }
            }
            let Some(out) = next else {
                return handed;
            };
            let client = client.clone();
            self.sending_first = out.first;
            self.sending = Some(Box::pin(async move {
                let Out { topic, retain, payload, properties, cleanup, .. } = out;
                // WAIT: mqtt-request
                let sent = client.publish_with_properties(topic.clone(), QoS::AtLeastOnce, retain, payload, properties).await;
                (topic, sent, cleanup)
            }));
        }
    }
}

/// The shutdown's flush: `Active=false` and DISCONNECT, polling until it is written, within
/// [`FLUSH_BOUND`]. Skipped when the session holds no connection: the last will says it then.
/// `outputs` holds what the stop cut short of each device's last batch: published with what is
/// pending.
async fn flush(shared: &Shared, client: &AsyncClient, incoming: &mut Incoming, outputs: &mut Outputs, host: &mut Lifecycle) {
    // A connection the broker accepted is one to flush on, whether or not its recovery has lasted
    // long enough to close the outage episode (second confirming review 2: a stop within the
    // stability window skipped every answer). The pump's word, not the session's: a CONNACK the
    // pump has read counts though the stop won the session's select over it, and a connection the
    // pump has lost does not (third confirming reviews, Claude minor 1).
    if !incoming.connected() {
        info!(kind = "bridge_shutdown_cut", "The broker is out of reach at shutdown; the last will says Active=false");
        return;
    }
    // The ack wait's deadline leaves the DISCONNECT and the close their part of the bound.
    let ack_deadline = Instant::now() + FLUSH_BOUND - DISCONNECT_PART;
    let flushed = async {
        // What still waits for each device will not run: refused, on Error and on its response
        // topic; then what is pending — Status, Errors, replies — then the last words (re-gate
        // R15). Here, in the session's own flush, so nothing else publishes them first by chance.
        // Refusals go to the reports' uncapped answers, which `take` gives first: a full mailbox
        // cannot overflow them (Codex final gate 2). Behind what the stop cut short of the last
        // batch — its publish under way finished first, then its answers, then these (third
        // confirming reviews, Codex 1). Every device's answers before any device's model, the
        // outbox's with them.
        // The model of a device not announced on this connection is not published (the rotation's
        // rule): its read-back — the fixed instance's before `Active=true`, a configured one's
        // before its grant — is still reading the retained State it would overwrite (the R2
        // confirming reviews 2, Claude minor 3). Its answers and unsolicited reports still go.
        for slot in &mut outputs.slots {
            crate::worker::report_refused(&slot.device.reports, slot.device.mailbox.disconnected(), STOPPED_BEFORE);
            slot.batch.add(&slot.device, slot.device.reports.take_sealed());
            if !slot.announced {
                let unpublished = slot.batch.drain(Part::Model).len();
                if unpublished > 0 {
                    debug!(unpublished, "A device's model not announced on this connection is not published at the stop");
                }
            }
        }
        // A SUBSCRIBE or UNSUBSCRIBE outstanding: no more is published than the ids left before its
        // packet id would come round (MQTT §2.2.1; R2 gate, Codex 3) — a fixed bridge's as a host's.
        // What does not fit waits for an acknowledgement, until the ack wait's deadline; past it
        // nothing more goes — no `Active=false` and no DISCONNECT, so the last will says
        // `Active=false` — rather than the rest on ids still in use (the R2 confirming reviews).
        loop {
            let ctx = Ctx { shared, client, line: incoming.line() };
            let mut left = host.flush_budget(&ctx).unwrap_or(usize::MAX);
            for part in [Part::Answers, Part::Model, Part::Unsolicited] {
                // WAIT: mqtt-request
                left -= outputs.outbox.publish(shared, client, &[part], left).await;
                for slot in &mut outputs.slots {
                    // WAIT: mqtt-request
                    left -= slot.batch.publish(shared, client, &[part], left).await;
                }
            }
            let published = outputs.outbox.is_empty() && outputs.slots.iter().all(|slot| slot.batch.is_empty());
            if published && left > 0 {
                break;
            }
            // WAIT: shutdown-flush
            match tokio::time::timeout_at(ack_deadline.into(), incoming.recv()).await {
                Ok(Some(
                    event @ (PumpEvent::SubAck { .. } | PumpEvent::UnsubAck { .. } | PumpEvent::Subscribing(_) | PumpEvent::Unsubscribing(_)),
                )) => host.settle_ack(&event),
                Ok(Some(PumpEvent::Ended(_) | PumpEvent::Disconnected) | None) => return Err("the connection ended".to_owned()),
                Err(_) => {
                    return Err(
                        "a subscription's acknowledgement did not come in time: what waits would take a packet id still in use, and is not published"
                            .to_owned(),
                    )
                }
                Ok(Some(_)) => {}
            }
        }
        emit(shared, client, shared.topics.active(), true, b"false".to_vec(), properties()).await;
        // Before the DISCONNECT, until the ack wait's deadline: every publish this session enqueued
        // written by the pump — a watermark, so the pump's count cannot be read stale before it
        // has taken the last ones — and none of them unacknowledged: the broker has the answers.
        // Past the deadline the DISCONNECT goes anyway, best effort (confirming review N1, second
        // confirming review minors). The pump's end ends the wait too: nothing more will be
        // written or acknowledged (third confirming reviews, Claude minor 2) — a publish rumqttc
        // refuses as it writes it (larger than the broker's maximum packet size) among the ways:
        // it never counts as written, and it fails rumqttc's poll, which ends the pump (minor 4).
        let acked = async {
            loop {
                let progress = incoming.progress();
                tokio::pin!(progress);
                progress.as_mut().enable();
                if incoming.ended() || (incoming.written() >= shared.enqueued.load(Ordering::SeqCst) && incoming.inflight() == 0) {
                    return;
                }
                // WAIT: mqtt-pump-queue
                progress.await;
            }
        };
        // WAIT: shutdown-flush
        if tokio::time::timeout_at(ack_deadline.into(), acked).await.is_err() {
            info!(
                kind = "bridge_shutdown_cut",
                unacknowledged = incoming.inflight(),
                "The broker did not acknowledge the shutdown's publishes in time; disconnecting anyway"
            );
        }
        if incoming.ended() {
            return Err("the connection ended".to_owned());
        }
        incoming.expect_disconnect();
        // WAIT: mqtt-request
        client.disconnect().await.map_err(|e| e.to_string())?;
        loop {
            // WAIT: mqtt-pump-queue
            match incoming.recv().await {
                Some(PumpEvent::Disconnected) | Some(PumpEvent::Ended(_)) | None => return Ok::<(), String>(()),
                Some(_) => {}
            }
        }
    };
    // WAIT: shutdown-flush
    match tokio::time::timeout(FLUSH_BOUND, flushed).await {
        Ok(Ok(())) => debug!("Active=false and DISCONNECT written"),
        Ok(Err(error)) => info!(
            kind = "bridge_shutdown_cut",
            error = %error,
            "The shutdown's flush ended before its DISCONNECT; the last will says Active=false"
        ),
        Err(_) => info!(
            kind = "bridge_shutdown_cut",
            bound_ms = FLUSH_BOUND.as_millis() as u64,
            "The shutdown's flush ran out its bound; the last will says Active=false"
        ),
    }
}

/// An incoming publish's topic, and the span it is handled in (its trace's remote parent, if it
/// carries one). The test seam's panic, if armed for the topic.
pub(crate) fn traced(publish: &Publish, owner: &str) -> (String, Span) {
    let topic = String::from_utf8_lossy(&publish.topic).into_owned();
    panic_for_test(&topic);
    let traceparent = wire::incoming_traceparent(publish.properties.as_ref());
    let instance = topic.split('/').nth(2).unwrap_or(owner);
    let bridge = (instance != owner).then_some(owner);
    let span = if matches!(topic.split('/').nth(1), Some("Command" | "Desired" | "Config")) {
        tracing::info_span!(parent:None,"mqtt_request",topic=%topic,instance,bridge)
    } else {
        tracing::debug_span!("mqtt_request",topic=%topic)
    };
    if let Some(tp) = &traceparent {
        tracing_init::traceparent::set_remote_parent(&span, tp);
    }
    (topic, span)
}

/// An incoming publish for `device`, received at `now`, handled without waiting: requests go to the
/// mailbox, what must be answered to the reports. Every deadline counts from `now`.
pub(crate) fn route(device: &Device, topic: &str, publish: &Publish, now: Instant) {
    route_with_config_admission(device, topic, publish, now, |_, _, _| {});
}

/// Configuration bookkeeping observes successful wire/table/mailbox admission,
/// never an invalid delivery and never a later driver/hardware verdict.
pub(crate) fn route_with_config_admission(
    device: &Device,
    topic: &str,
    publish: &Publish,
    now: Instant,
    mut admitted: impl FnMut(&str, &crate::RequestContext, bool),
) {
    let properties = publish.properties.as_ref();
    let original_context = wire::request_context(properties).unwrap_or_else(|_| wire::trace_context(properties));
    let payload = &publish.payload[..];
    let malformed = |error: String, report: ErrorReport| {
        info!(kind = "validation_rejected", topic, error = %error, "A request was malformed; answered on Error");
        device.reports.error(ErrorReport { error, ..report });
    };
    let blank = || ErrorReport::new(Reason::Malformed, "").context(&original_context);
    match device.topics.parse(topic) {
        Inbound::State { target } => {
            if publish.retain {
                match wire::decode_state(payload).and_then(|values| Ok((values, wire::request_context(properties)?))) {
                    Ok((values, context)) => device.reports.read_back_with_context(&target, values, &context),
                    Err(e) => debug!(target = %target, error = %e, "The retained state read back is not a state; ignored"),
                }
            }
        }
        Inbound::DesiredProperty { target, property } => {
            let report = blank().target(Some(&target)).property(&property);
            match wire::decode_desired_property(payload) {
                Err(e) => malformed(e, report),
                Ok(None) => {
                    // The key's assumption is settled — unless a write of it is still unproven: the
                    // probe proves it, or the link's loss holds it again (re-gate 4 minor 7).
                    match wire::request_context(properties) {
                        Err(error) => malformed(error, report),
                        Ok(context) => match device.mailbox.retract_with_context(&target, &property, &context) {
                            Err(why) => device.reports.request_error(
                                ErrorReport::new(Reason::Refused, why.as_str()).context(&context).target(Some(&target)).property(&property),
                            ),
                            Ok(false) => device.reports.assumption_settled_with_context(&target, &property, &context),
                            Ok(true) => {}
                        },
                    }
                }
                Ok(Some(value)) => match wire::request_meta(properties, now) {
                    Err(e) => malformed(e, report),
                    Ok(meta) => {
                        // A replay's "before" basis, as of its arrival (re-gate R2).
                        let basis = publish.retain.then(|| device.reports.broker_basis_with_context(&target, &property, &meta.context));
                        let value = StateValue {
                            context: meta.context,
                            target,
                            property,
                            value,
                            deadline: meta.deadline,
                            conflict: meta.conflict,
                            check: Check::default(),
                            basis,
                            reheld: false,
                        };
                        let target = value.target.clone();
                        let property = value.property.clone();
                        if let Posted::Refused(why) = withdrawing(device, &original_context, |withdraw| {
                            device.mailbox.post_state_then(value, publish.retain, withdraw)
                        }) {
                            device.reports.request_error(
                                ErrorReport::new(Reason::Refused, why.as_str())
                                    .context(&original_context)
                                    .target(Some(&target))
                                    .property(&property),
                            );
                        }
                    }
                },
            }
        }
        Inbound::DesiredTarget { target } => {
            let report = blank().target(Some(&target));
            if publish.retain {
                return malformed("a target-level Desired is never retained".into(), report);
            }
            match wire::decode_desired_target(payload).and_then(|values| Ok((values, wire::request_meta(properties, now)?))) {
                Err(e) => malformed(e, report),
                Ok((values, meta)) => {
                    let values = values
                        .into_iter()
                        .map(|(property, value)| StateValue {
                            context: meta.context.clone(),
                            target: target.clone(),
                            property,
                            value,
                            deadline: meta.deadline,
                            conflict: meta.conflict,
                            check: Check::default(),
                            basis: None,
                            reheld: false,
                        })
                        .collect();
                    if let Posted::Refused(why) =
                        withdrawing(device, &original_context, |withdraw| device.mailbox.post_target_then(values, withdraw))
                    {
                        device.reports.request_error(
                            ErrorReport::new(Reason::Refused, why.as_str()).context(&original_context).target(Some(&target)),
                        );
                    }
                }
            }
        }
        Inbound::Command => {
            let reply = ReplyTo::from_properties(properties, &device.topics);
            // A command refused as it arrives still answers on its response topic, if it named
            // one the bridge can publish to (review C13).
            let refuse = |error: String| {
                if let Ok(Some(reply)) = &reply {
                    let (target, command) = wire::command_names(payload);
                    device.reports.event(Outbound {
                        context: reply.context(&original_context),
                        topic: Some(reply.topic.clone()),
                        payload: reply_payload(
                            target.as_deref(),
                            &command,
                            &Answer::Failed { reason: Reason::Malformed, error: error.clone() },
                        ),
                        correlation: reply.correlation.clone(),
                    });
                }
                malformed(error, blank());
            };
            if publish.retain {
                return refuse("a command is never retained".into());
            }
            let reply = match &reply {
                Err(e) => return malformed(e.clone(), blank()),
                Ok(reply) => reply.clone(),
            };
            let (target, call) = match wire::decode_command(payload) {
                Err(e) => return refuse(e),
                Ok(decoded) => decoded,
            };
            let context = match wire::request_context(properties) {
                Ok(context) => context,
                Err(error) => return refuse(error),
            };
            command(
                device,
                target,
                call,
                reply,
                properties.and_then(|p| p.message_expiry_interval).map(|s| now + Duration::from_secs(u64::from(s))),
                context,
            );
        }
        Inbound::Config { target } => {
            match wire::decode_config(payload).and_then(|config| Ok((config, wire::request_context(properties)?))) {
                Err(e) => malformed(e, blank().target(Some(&target))),
                Ok((config, context)) => {
                    let present = config.is_some();
                    match device.mailbox.post_config_with_context(&target, config, context.clone()) {
                        Posted::Refused(why) => device.reports.request_error(
                            ErrorReport::new(Reason::Refused, why.as_str()).context(&original_context).target(Some(&target)),
                        ),
                        Posted::Queued | Posted::Held | Posted::Skipped => admitted(&target, &context, present),
                        Posted::Dropped => {}
                    }
                }
            }
        }
        // A fixed bridge never subscribes to its `Status`: one arriving is no topic it reads.
        Inbound::Status | Inbound::Unknown => malformed(format!("`{topic}` is no topic of this bridge's grammar"), blank()),
    }
}

/// A command: the standard `refresh` and `assume`, or one the driver declares.
fn command(
    device: &Device,
    target: Option<String>,
    call: crate::driver::CommandCall,
    reply: Option<ReplyTo>,
    deadline: Option<Instant>,
    context: crate::RequestContext,
) {
    let answer = |answer: Answer| {
        if let Some(reply) = &reply {
            device.reports.event(Outbound {
                context: reply.context(&context),
                topic: Some(reply.topic.clone()),
                payload: reply_payload(target.as_deref(), &call.command, &answer),
                correlation: reply.correlation.clone(),
            });
        }
    };
    let refused = |reason: Reason, error: &str| {
        device.reports.request_error(ErrorReport::new(reason, error).context(&context).target(target.as_deref()).command(&call.command));
        answer(Answer::Failed { reason, error: error.to_owned() });
    };
    // Past its expiry when the session reads it — it waited behind a stalled broker — a command
    // is refused, whatever its class: `assume` and the reads included (review R9).
    if deadline.is_some_and(|d| Instant::now() >= d) {
        debug!(command = %call.command, "A command past its expiry when read; refused");
        return refused(Reason::Refused, "the command reached its expiry before the bridge read it; not executed");
    }
    match call.command.as_str() {
        "refresh" => {
            let posted =
                device.mailbox.post_read_with_context(ReadKey::Refresh(target.clone()), None, reply.clone(), deadline, context.clone());
            if posted == Posted::Dropped {
                refused(Reason::Refused, "the device is not connected");
            }
        }
        "assume" if device.reports.fenced() => refused(Reason::Refused, "assume is unavailable for a generation-fenced device"),
        "assume" => match &target {
            // The Store's input: a property that cannot stand as a topic segment is malformed, not
            // a driver's contract violation (Claude's re-gate minor 6).
            Some(_) if call.params.keys().any(|p| !crate::topics::valid_segment(p)) => {
                info!(kind = "validation_rejected", command = "assume", "`assume` names a property that cannot stand as a topic segment");
                refused(Reason::Malformed, "`assume` names a property that cannot stand as a topic segment");
            }
            Some(t) if !call.params.is_empty() => {
                device.reports.state_with_context(t, call.params.clone(), true, &context);
                answer(Answer::Done(None));
            }
            _ => {
                info!(kind = "validation_rejected", command = "assume", "`assume` names a target and the values to assume");
                refused(Reason::Malformed, "`assume` names a target and the values to assume");
            }
        },
        // A device whose driver is not built yet takes no command: as one not connected.
        _ if device.commands.get().is_none() => refused(Reason::Refused, "the device is not connected"),
        name => match device.commands.get().into_iter().flatten().find(|c| c.name == name).map(|c| c.class) {
            None => {
                info!(command = name, "A command the driver does not declare; answered on Error");
                refused(Reason::Unsupported, "no such command");
            }
            Some(CommandClass::Read) => {
                let key = ReadKey::Command { target: target.clone(), command: name.to_owned() };
                if device.mailbox.post_read_with_context(key, Some(call.clone()), reply.clone(), deadline, context.clone())
                    == Posted::Dropped
                {
                    refused(Reason::Refused, "the device is not connected");
                }
            }
            Some(CommandClass::Momentary | CommandClass::WithReply) => {
                if let Posted::Refused(why) =
                    device.mailbox.post_command_with_context(target.clone(), call.clone(), reply.clone(), deadline, context.clone())
                {
                    refused(Reason::Refused, why.as_str());
                }
            }
        },
    }
}

/// The test seam for a panicking session task (with the `test-seams` feature only; a build without it has none): when
/// `MQTT_BRIDGE_TEST_PANIC_TOPIC` names a topic, a publish on it panics the session. Unset, it does
/// nothing.
#[cfg(feature = "test-seams")]
fn panic_for_test(topic: &str) {
    static SEAM: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    if SEAM.get_or_init(|| std::env::var(TEST_PANIC_TOPIC).ok()).as_deref() == Some(topic) {
        panic!("the test seam's panic, on a publish on {topic}");
    }
}

#[cfg(not(feature = "test-seams"))]
fn panic_for_test(_: &str) {}

/// A request's `post`, given the withdrawal of the assumption of each write held again before
/// proof that it replaces: withdrawn at once, before a retraction of the new request could settle
/// it as proven (second confirming review 1), and in the same step as the replacement, under the
/// mailbox's lock — mailbox, then reports, the one order — before the worker can take the
/// replacement and assume it (third confirming reviews, Codex 2: withdrawn after, it reverted the
/// replacement's own assumption). Logged once both locks are released. The worker withdraws the
/// writes its own takes drop at their deadline.
fn withdrawing<R>(device: &Device, context: &crate::RequestContext, post: impl FnOnce(&mut dyn FnMut(&str, &str)) -> R) -> R {
    let mut withdrawn: Vec<(String, String)> = Vec::new();
    let result = post(&mut |target, property| {
        if device.reports.withdraw_assumption_with_context(target, property, context) {
            withdrawn.push((target.to_owned(), property.to_owned()));
        }
    });
    after_post(device);
    for (target, property) in withdrawn {
        info!(target, property, "A write the device never proved taken was replaced before proof; its assumed value is withdrawn");
    }
    result
}

/// (tests) What runs after a post.
#[cfg(test)]
type AfterPost = Option<Box<dyn FnMut(&Device)>>;

#[cfg(test)]
thread_local! {
    /// (tests) Run on the session's thread right after a request's post returns: the worker's turn,
    /// placed deterministically.
    pub(crate) static AFTER_POST: std::cell::RefCell<AfterPost> = const { std::cell::RefCell::new(None) };
}

/// The test seam after a post (tests only; nothing otherwise).
fn after_post(_device: &Device) {
    #[cfg(test)]
    AFTER_POST.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook(_device);
        }
    });
}

/// What a stop refuses: a command still waiting for the device.
pub(crate) const STOPPED_BEFORE: &str = "the bridge stopped before it ran";

/// Spawn the MQTT side on the current runtime.
pub(crate) fn spawn(
    shared: Arc<Shared>,
    host: Lifecycle,
    outputs: Outputs,
    events: mpsc::UnboundedReceiver<Event>,
    shutdown: watch::Receiver<bool>,
    set: &mut tokio::task::JoinSet<()>,
) {
    set.spawn(run(shared, host, outputs, events, shutdown).in_current_span());
}

#[cfg(test)]
mod tests;

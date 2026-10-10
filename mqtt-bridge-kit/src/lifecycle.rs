//! The multi-device host's per-instance lifecycle (multi-device spec §3, §4.2–§4.6): the router,
//! the ownership by `Config/{instance}`, the per-instance subscriptions and their bootstrap, the
//! stop path, the retained topics' tags, healing and cleanup, and the startup's `Active` gate —
//! the session part of a host, driven by the session's events ([`crate::session`]).
//!
//! **Ownership** (§3, §4.2). `Config/+` and then a barrier, `{Root}/_barrier/{bridge id}`, are
//! subscribed on every CONNACK; the barrier's SUBACK closes the root replay. An instance is owned
//! iff its last accepted Config names this process (`via`, or omitted, the kind the Config names)
//! and its kind.
//!
//! **Bootstrap** (§4.3). S1 (`State/{i}/+`, `Status/{i}`: the read-back, kept for healing), then
//! S2 (`Config/{i}/+`; its SUBACK closes S1's replay and freezes the read-back), then S3 (the
//! requests; its SUBACK closes S2's replay, and the device becomes eligible to execute). Each
//! SUBACK is matched by packet id; refused or overdue ([`GRANT_BOUND`]) ends the session. The
//! driver is built off to the side ([`crate::devices`]); the worker starts once both are done.
//!
//! **Stopping a device** (§4.4): admission closes at once (late deliveries get the terminal
//! policy), the worker stops (R1's stop), its output retires through the rotation, then its
//! subscriptions go, each UNSUBACK matched by packet id; only then does a successor bootstrap.
//!
//! **Retained topics** (§4.5): every `Status`/`State` of an instance carries the `via` tag; a
//! foreign write to, or a retraction of, an owned slot is healed; a delete retracts the topics whose
//! current value carries this process's own tag.
//!
//! **The fixed instance — the host's N = 1 mode** (§4.1, §4.2). A [`Bridge`](crate::Bridge) is
//! this lifecycle with one fixed instance, named from `--instance` and served from the start
//! (`Lifecycle::start_fixed`): its owner is the process itself, so it has no Config, no `via`
//! tag, no healing and no cleanup, and a fixed host subscribes no `Config/+`, no barrier and no
//! `Status/+`. Its CONNACK duties are R1's (Bridge Runtime spec §5, §9), in R1's order: once per
//! process the read-back, `State/{i}/#`, alone, its SUBACK awaited within
//! [`READ_BACK_WAIT`](crate::session::READ_BACK_WAIT) (refused or late: WARN `read_back_incomplete`
//! and on without it; the next SUBACK completes it, and it is unsubscribed); then its four request
//! subscriptions, `Config/{i}/#`, `Desired/{i}/+`, `Desired/{i}/+/+`, `Command/{i}`; once they are
//! handed over, `Active=true`, the `Version`, and its whole retained model. Every SUBSCRIBE,
//! UNSUBSCRIBE and publish of it goes through the same packet-id budget as a configured instance's
//! (`Conn`): one runtime, one budget.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{Filter, Publish};
use rumqttc::v5::mqttbytes::QoS;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{debug, info, warn, Span};

use crate::config::{self, TimingOverrides};
use crate::devices::{drop_off_thread, Event, Spawner, Startable};
use crate::driver::{DriverInfo, PropertyMap};
use crate::host::{DeviceConfig, Factory, FactoryError};
use crate::mailbox::Mailbox;
use crate::pump::PumpEvent;
use crate::replies::{reply_payload, Answer, ReplyTo};
use crate::reports::{LinkStatus, Reports};
use crate::session::{self, Batch, Ctx, Device, Failed, Flow, Out, Outputs, Part};
use crate::topics::{Inbound, Topics};
use crate::wire::{self, ErrorReport, Reason};
use crate::MqttTiming;

/// How long a subscription's SUBACK, or an unsubscription's UNSUBACK, may be outstanding before the
/// session ends (multi-device spec §4.3, §4.4): time alone never establishes absence, and a grant
/// pending is never bypassed.
pub const GRANT_BOUND: Duration = Duration::from_secs(10);

/// After the `Config/+` grant, how long `Active=true` waits for the barrier's SUBACK before it goes
/// without it (multi-device spec §4.6); an instance whose Config arrives later gets its `Status`
/// as it is acquired. An incomplete replay releases nothing.
pub const REPLAY_FALLBACK: Duration = Duration::from_secs(5);

/// The floor of a failed construction's retry pacing: each retry spends a thread on the factory.
const CONSTRUCTION_RETRY_FLOOR: Duration = Duration::from_secs(1);

/// How often a failed instance's held requests are swept for their deadlines.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// The host's own fields of a Config: everything else is the driver's.
const HOST_FIELDS: [&str; 3] = ["bridge", "via", "disabled"];

/// What a `Config/{instance}` says, as this process reads it (multi-device spec §3).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    /// Empty: the instance is deleted.
    Deleted,
    /// Its `via` cannot be read (not JSON, not an object, a `via` that is not a string): no owner
    /// can be identified.
    Unreadable(String),
    /// It names another process (`via`, or the default bridge id of the kind it names).
    Foreign(String),
    /// It names this process, and another kind.
    KindMismatch { config: Value, kind: String },
    /// It names this process, and the host's own fields are invalid.
    Malformed { config: Value, error: String },
    /// This process's: the parsed, validated Config — its owner, `disabled` and the driver's fields,
    /// in one canonical form, so that equality is on what it says (§3: `disabled: false` written
    /// out or left out, an explicit default `via` or an omitted one, are the same Config) — whether
    /// it is disabled, and the driver's fields.
    Ours { config: Value, disabled: bool, fields: PropertyMap },
}

/// Read a `Config/{instance}` payload as process `id` of `kind` does (multi-device spec §3, §4.2).
pub(crate) fn classify(payload: &[u8], kind: &str, id: &str) -> Verdict {
    if payload.is_empty() {
        return Verdict::Deleted;
    }
    let config: Value = match serde_json::from_slice(payload) {
        Ok(config) => config,
        Err(e) => return Verdict::Unreadable(format!("not JSON: {e}")),
    };
    let Value::Object(fields) = &config else {
        return Verdict::Unreadable("not a JSON object".into());
    };
    // The effective owner: `via`, else the default bridge id of the kind the Config names — that
    // kind itself (§4.2). Not this process's kind: a default process of another kind sharing the
    // root would take the instance for its own (R2 gate, Codex 9). Naming neither, it names no
    // owner.
    let owner = match (fields.get("via"), fields.get("bridge")) {
        (Some(Value::String(via)), _) => via.clone(),
        (Some(_), _) => return Verdict::Unreadable("its `via` is not a string".into()),
        (None, Some(Value::String(named))) => named.clone(),
        (None, _) => return Verdict::Unreadable("it names neither a process (`via`) nor a kind (`bridge`, a string)".into()),
    };
    if owner != id {
        return Verdict::Foreign(owner);
    }
    let named = match fields.get("bridge") {
        Some(Value::String(named)) => named.clone(),
        _ => return Verdict::Malformed { config: config.clone(), error: "the Config names no kind (`bridge`, a string)".into() },
    };
    if named != kind {
        return Verdict::KindMismatch { config: config.clone(), kind: named };
    }
    let disabled = match fields.get("disabled") {
        None => false,
        Some(Value::Bool(disabled)) => *disabled,
        Some(_) => return Verdict::Malformed { config: config.clone(), error: "`disabled` is not a boolean".into() },
    };
    let fields: PropertyMap =
        fields.iter().filter(|(k, _)| !HOST_FIELDS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect();
    // The canonical form: every host field written out.
    let mut canonical = fields.clone();
    canonical.insert("bridge".into(), Value::String(named));
    canonical.insert("via".into(), Value::String(owner));
    canonical.insert("disabled".into(), Value::Bool(disabled));
    Verdict::Ours { config: Value::Object(canonical), disabled, fields }
}

/// The driver's fields of an accepted Config.
fn driver_fields(config: &Value) -> PropertyMap {
    config
        .as_object()
        .map(|o| o.iter().filter(|(k, _)| !HOST_FIELDS.contains(&k.as_str())).map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}

/// What an instance's latest Config asks of this process.
#[derive(Debug, Clone, PartialEq)]
enum Want {
    /// Serve a device built from this Config.
    Serve(Value),
    /// Own the instance, disabled: no device, its read-back kept, `Status=disabled`.
    Disable(Value),
    /// Owned, but nothing new to build: the Config naming this process is malformed, and the last
    /// accepted one stays in force — none: the instance is failed, `Status=unreachable`.
    Broken,
    /// Release it: deleted (`cleanup`: its own tagged topics retracted) or no longer this
    /// process's.
    Leave { cleanup: bool },
    /// The fixed instance (N = 1): served from the start, never configured, never stopped.
    Fixed,
}

/// One of an instance's subscriptions, on the current connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sub {
    None,
    Requested,
    Granted,
}

/// After a device's stop, what the instance becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Then {
    /// A successor, from the driver built for the new Config, after a fresh bootstrap.
    Successor,
    /// Disabled: S2 and S3 unsubscribed, S1 kept.
    Disable,
    /// Released: every subscription gone; `cleanup`, its own tagged topics retracted.
    Release { cleanup: bool },
}

/// Where a device's stop has got (multi-device spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// The worker is asked to stop.
    Worker,
    /// Its output retires through the rotation.
    Retire,
    /// Its subscriptions are going, the UNSUBACKs awaited.
    Unsubscribe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Bootstrapping or serving (a failed construction included: a shell with no worker).
    Live,
    /// Disabled: S1 only, `Status=disabled`.
    Disabled,
    /// Admission closed; the stop under way.
    Stopping { step: Step, then: Then },
}

/// One device runtime of an instance: at most one at a time.
struct Rt {
    id: u64,
    device: Arc<Device>,
    phase: Phase,
    /// A worker runs.
    worker: bool,
    read: Sub,
    setup: Sub,
    requests: Sub,
    /// S3 has been granted once: the device is eligible to execute, and a later bootstrap's S2
    /// replay is compared with the installed target Configs.
    bootstrapped: bool,
    /// UNSUBACKs awaited.
    unsubscribing: usize,
}

/// A driver built for a Config, waiting to start.
struct Ready {
    config: Value,
    driver: Box<dyn Startable>,
    info: DriverInfo,
}

/// What an instance's read-back last saw on one of its topics: the `via` (exactly one, or none)
/// and the payload.
struct Seen {
    via: Option<String>,
    payload: Vec<u8>,
}

/// An instance this process owns.
struct Instance {
    name: String,
    topics: Topics,
    /// The last accepted Config: the one in force.
    accepted: Option<Value>,
    /// The last Config naming this process, accepted or not: an identical one is a no-op.
    last_seen: Option<Value>,
    /// The last unreadable payload said (one WARN `device_config_invalid` each).
    invalid: Option<Vec<u8>>,
    want: Want,
    rt: Option<Rt>,
    /// A construction under way: its id and Config.
    build: Option<(u64, Value)>,
    ready: Option<Ready>,
    /// A Config whose construction failed: not built again until it changes, or — with nothing
    /// serving — the reconnect pacing's retry.
    failed_config: Option<Value>,
    retry_at: Option<Instant>,
    /// A disabled Config whose changed driver fields the factory has accepted: the disable may go
    /// on (§3: validated before it is accepted).
    validated: Option<Value>,
    /// The read-back's view of the instance's slot: each topic's current retained value as last
    /// seen.
    slot: BTreeMap<String, Seen>,
    /// The target Configs given to the current device (non-empty last).
    installed: BTreeSet<String>,
    /// The target Configs present on the current connection since S2 was asked for: replayed by
    /// S2, or written live meanwhile (a live retraction removes one).
    replayed: BTreeSet<String>,
}

impl Instance {
    /// A newly owned instance: no Config accepted yet, no runtime, its slot unread.
    fn new(name: &str, topics: Topics, want: Want) -> Instance {
        Instance {
            name: name.to_owned(),
            topics,
            accepted: None,
            last_seen: None,
            invalid: None,
            want,
            rt: None,
            build: None,
            ready: None,
            failed_config: None,
            retry_at: None,
            validated: None,
            slot: BTreeMap::new(),
            installed: BTreeSet::new(),
            replayed: BTreeSet::new(),
        }
    }

    /// Failed: a live runtime with no worker and no driver waiting to start — nothing serves the
    /// instance — whose construction has failed (or whose Config naming this process is malformed,
    /// with none accepted before). A retry's construction under way is no service: the failed
    /// policy holds until a device serves (R2 gate, Codex 10). A first construction not yet done
    /// has not failed.
    fn failed(&self) -> bool {
        self.rt.as_ref().is_some_and(|rt| rt.phase == Phase::Live && !rt.worker)
            && self.ready.is_none()
            && (self.failed_config.is_some() || self.want == Want::Broken)
    }
}

/// What a request awaiting its acknowledgement is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Purpose {
    Configs,
    Barrier,
    Read {
        instance: String,
        rt: u64,
    },
    Setup {
        instance: String,
        rt: u64,
    },
    Requests {
        instance: String,
        rt: u64,
    },
    Unsub {
        instance: String,
        rt: u64,
    },
    /// The process's first root replay of `Status/+`, closed by the barrier: its own litter.
    Statuses,
    /// Its unsubscription, once read.
    StatusesDone,
    /// A cleanup's read-back of an instance this process does not own (`Sweep`).
    Sweep {
        instance: String,
    },
    /// Its unsubscriptions: the last closes its replay.
    SweepDone {
        instance: String,
    },
}

/// A cleanup's short read-back of an instance this process does not own (§4.5): `State/{i}/+` and
/// `Status/{i}` subscribed, then unsubscribed — the last UNSUBACK closes the replay — and only what
/// still carries this process's own tag is retracted.
#[derive(Default)]
struct Sweep {
    /// Each topic's retained value as replayed: its `via` (exactly one, or none).
    seen: BTreeMap<String, Option<String>>,
    /// UNSUBACKs awaited.
    closing: usize,
}

/// The process's first root replay of `Status/+` (§4.5, litter of instances deleted while it was
/// down): not read yet, being read (the instances whose retained `Status` carries its own tag), or
/// done.
enum Scan {
    Pending,
    Reading(BTreeSet<String>),
    Done,
}

/// A SUBSCRIBE or UNSUBSCRIBE the lifecycle asks for.
enum Request {
    Subscribe(Vec<Filter>),
    Unsubscribe(String),
}

/// A request not yet handed to rumqttc (`Lifecycle::drive` hands them over).
struct Control {
    purpose: Purpose,
    request: Request,
}

impl Control {
    fn is_unsubscribe(&self) -> bool {
        matches!(self.request, Request::Unsubscribe(_))
    }
}

/// A request handed to rumqttc and awaiting its acknowledgement: its bound, and where it stands in
/// the order rumqttc hands out packet ids (`Lifecycle::clock`).
struct Pending {
    purpose: Purpose,
    unsubscribe: bool,
    deadline: Instant,
    clock: u64,
}

/// One connection's subscriptions.
///
/// **Packet ids** (MQTT §2.2.1: an id is not used again until its acknowledgement; R2 gate, Codex
/// 3). rumqttc 0.25 draws SUBSCRIBE, UNSUBSCRIBE and publish ids from one counter, in the order
/// the requests reach it, cycling through [`crate::pump::Line::id_space`] ids (the broker's receive
/// maximum), and checks only its publishes against the ids in use: a publish, or a later SUBSCRIBE,
/// could take the id of a SUBSCRIBE still awaiting its SUBACK. So the lifecycle keeps the order
/// itself: a request is handed over only when no publish is unacknowledged (the line is clear —
/// nothing is published while one waits for it), and while any request is outstanding, fewer ids
/// are handed out after the oldest than the id space holds — requests and publishes alike, the
/// publication budgeted to match. No id in use is handed out again, and each acknowledgement
/// matches its one request by its packet id.
#[derive(Default)]
struct Conn {
    connected: bool,
    /// Requests not yet handed to rumqttc, in order.
    waiting: VecDeque<Control>,
    /// Requests handed to rumqttc whose packet id the pump has not seen written yet, in order.
    unnumbered: VecDeque<Pending>,
    /// Requests awaiting their acknowledgement, by packet id (unique: see above).
    numbered: BTreeMap<u16, Pending>,
    /// The requests handed to rumqttc on this connection.
    controls: u64,
    /// The next request waits for the line to clear of publishes.
    line_wait: bool,
    configs_granted: Option<Instant>,
    barrier_acked: bool,
    /// Instances whose `Config/{i}` came on this connection.
    replayed: BTreeSet<String>,
    active: bool,
}

impl Conn {
    /// Every request handed to rumqttc and not yet acknowledged.
    fn outstanding(&self) -> impl Iterator<Item = &Pending> {
        self.unnumbered.iter().chain(self.numbered.values())
    }

    /// The subscriptions requested and not yet granted, the barrier's aside: `Active=true` waits
    /// for every other grant, and for the barrier only up to the replay's fallback (multi-device
    /// spec §4.6) — counted here, the fallback could never apply.
    fn pending_subscriptions(&self) -> usize {
        let counted = |unsubscribe: bool, purpose: &Purpose| !unsubscribe && *purpose != Purpose::Barrier;
        let waiting = self.waiting.iter().filter(|c| counted(c.is_unsubscribe(), &c.purpose)).count();
        waiting + self.outstanding().filter(|p| counted(p.unsubscribe, &p.purpose)).count()
    }

    /// The outstanding requests' first bound: a waiting one has none yet — it waits for the line,
    /// which the broker's acknowledgements clear (the pump bounds those), or for an outstanding
    /// one's acknowledgement (bounded here).
    fn next_deadline(&self) -> Option<Instant> {
        self.outstanding().map(|p| p.deadline).min()
    }
}

/// The device id of the fixed instance's runtime: `Lifecycle::next` never hands it out.
const FIXED_ID: u64 = 0;

/// The fixed instance of a [`Bridge`](crate::Bridge) (the host's N = 1 mode; see the module
/// documentation): its name, and its read-back's progress — once per process, as R1's.
struct Fixed {
    name: String,
    /// The read-back's SUBACK has come, granted or refused, or did not come in time: it is not
    /// asked again. A session that ends before either leaves it to the next.
    read_back_done: bool,
    /// The read-back asked on this connection waits for its SUBACK until then.
    read_back_until: Option<Instant>,
}

/// What a host is, for its lifecycle.
pub(crate) struct Settings {
    pub kind: String,
    /// The bridge id: a fixed host's is its instance.
    pub id: String,
    pub root: String,
    /// The factory a configured instance's driver is built by; none for a fixed host.
    pub factory: Option<Factory>,
    pub factory_timeout: Duration,
    /// The pacing of a failed construction's retries.
    pub retry: Duration,
    pub overrides: TimingOverrides,
    pub device_stable: Option<Duration>,
    pub mqtt: MqttTiming,
}

/// See the module documentation.
pub(crate) struct Lifecycle {
    settings: Settings,
    /// The session part: the rotation's broker-loss list is its.
    shared: Arc<session::Shared>,
    spawner: Spawner,
    span: Span,
    ready: Arc<Notify>,
    instances: BTreeMap<String, Instance>,
    /// For an instance this process does not own, the last payload it rejected (a kind mismatch):
    /// said once.
    rejected: BTreeMap<String, Vec<u8>>,
    conn: Conn,
    next_id: u64,
    next_sweep: Option<Instant>,
    /// The bridge cleanups not yet done (a delete that ended while the broker was away, one lost
    /// with its connection, an instance deleted while the process was down): swept after the next
    /// root replay if the instance is not this process's again — its slot read afresh, and only
    /// what still carries the own tag retracted (multi-device spec §4.5; R2 gate, Codex 7).
    cleanups: BTreeSet<String>,
    /// The cleanups whose retractions are in this connection's outbox, with the session's publish
    /// count once the outbox has handed them over: done when the broker has acknowledged that many,
    /// pending again (`cleanups`) if the connection is lost first (R2 gate, Claude minor 2).
    queued_cleanups: Vec<(String, Option<u64>)>,
    /// The cleanups' read-backs under way, by instance.
    sweeps: BTreeMap<String, Sweep>,
    /// The first root replay's `Status/+` read.
    scan: Scan,
    /// A fixed host's one instance (N = 1).
    fixed: Option<Fixed>,
}

impl Lifecycle {
    pub(crate) fn new(settings: Settings, shared: Arc<session::Shared>, spawner: Spawner, span: Span, ready: Arc<Notify>) -> Lifecycle {
        Lifecycle {
            settings,
            shared,
            spawner,
            span,
            ready,
            instances: BTreeMap::new(),
            rejected: BTreeMap::new(),
            conn: Conn::default(),
            next_id: 1,
            next_sweep: None,
            cleanups: BTreeSet::new(),
            queued_cleanups: Vec::new(),
            sweeps: BTreeMap::new(),
            scan: Scan::Pending,
            fixed: None,
        }
    }

    /// Serve the one fixed instance `name` (a [`Bridge`](crate::Bridge), N = 1): R1's device —
    /// untagged, in `span` — its worker started now, whatever the broker does (R1: the device side
    /// runs from the start, its reports waiting for `Active`).
    pub(crate) fn start_fixed(
        &mut self,
        outputs: &mut Outputs,
        name: &str,
        driver: Box<dyn Startable>,
        info: DriverInfo,
        span: Span,
    ) -> Result<(), String> {
        let device = Arc::new(Device {
            topics: Topics::new(&self.settings.root, name)?,
            commands: info.commands.clone().into(),
            mailbox: Mailbox::new(),
            reports: Reports::with_ready(self.ready.clone()),
            tag: None,
            span: span.clone(),
        });
        self.spawner.start(FIXED_ID, name, false, driver, info, device.mailbox.clone(), device.reports.clone(), span)?;
        self.insert_fixed(outputs, device);
        Ok(())
    }

    /// The fixed instance, its device's worker running (or, in a unit test, none): owned by the
    /// process itself from the start, a live runtime that needs no bootstrap.
    pub(crate) fn insert_fixed(&mut self, outputs: &mut Outputs, device: Arc<Device>) {
        let name = device.topics.instance().to_owned();
        outputs.add(&self.shared, FIXED_ID, device.clone());
        let rt = Rt {
            id: FIXED_ID,
            device: device.clone(),
            phase: Phase::Live,
            worker: true,
            read: Sub::None,
            setup: Sub::None,
            requests: Sub::None,
            bootstrapped: true,
            unsubscribing: 0,
        };
        let instance = Instance { rt: Some(rt), ..Instance::new(&name, device.topics.clone(), Want::Fixed) };
        self.instances.insert(name.clone(), instance);
        self.fixed = Some(Fixed { name, read_back_done: false, read_back_until: None });
    }

    /// The fixed instance's name, for a fixed host.
    fn fixed_name(&self) -> Option<String> {
        self.fixed.as_ref().map(|f| f.name.clone())
    }

    /// Whether `name` is the fixed instance.
    fn is_fixed(&self, name: &str) -> bool {
        self.fixed.as_ref().is_some_and(|f| f.name == name)
    }

    /// Whether `purpose` is one of the fixed instance's subscriptions.
    fn fixed_purpose(&self, purpose: &Purpose) -> bool {
        match purpose {
            Purpose::Read { instance, .. }
            | Purpose::Setup { instance, .. }
            | Purpose::Requests { instance, .. }
            | Purpose::Unsub { instance, .. } => self.is_fixed(instance),
            _ => false,
        }
    }

    fn next(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    /// When a timer falls due: an outstanding grant's bound, the replay's fallback, a failed
    /// instance's retry, the sweep.
    pub(crate) fn next_wake(&self) -> Option<Instant> {
        // The fallback is a timer only until it passes: then `Active=true` waits for the grants
        // still pending, each SUBACK checking it — a wake at a past instant would spin.
        let fallback = (!self.conn.active)
            .then_some(self.conn.configs_granted.map(|at| at + REPLAY_FALLBACK))
            .flatten()
            .filter(|at| *at > Instant::now());
        let retries = self.instances.values().filter_map(|i| i.retry_at);
        let read_back = self.fixed.as_ref().and_then(|f| f.read_back_until);
        [self.conn.next_deadline(), fallback, self.next_sweep, read_back].into_iter().flatten().chain(retries).min()
    }

    // ---- Subscriptions ---------------------------------------------------------------------

    /// Subscribe `filters` for `purpose`: queued, handed to rumqttc in its turn (`drive`), then
    /// awaited.
    async fn request_subscription(&mut self, ctx: Option<&Ctx<'_>>, purpose: Purpose, filters: Vec<Filter>) -> Result<(), Failed> {
        if ctx.is_some() && self.conn.connected {
            self.conn.waiting.push_back(Control { purpose, request: Request::Subscribe(filters) });
        }
        Ok(())
    }

    /// Unsubscribe `filter` for `purpose`; whether it is asked for (a connection holds it).
    async fn request_unsubscription(&mut self, ctx: Option<&Ctx<'_>>, purpose: Purpose, filter: String) -> Result<bool, Failed> {
        if ctx.is_some() && self.conn.connected {
            self.conn.waiting.push_back(Control { purpose, request: Request::Unsubscribe(filter) });
            return Ok(true);
        }
        Ok(false)
    }

    /// Where the session stands in the order rumqttc hands out packet ids: every publish it enqueued
    /// and every request it handed over, on this connection.
    fn clock(&self, ctx: &Ctx<'_>) -> u64 {
        ctx.shared.enqueued.load(std::sync::atomic::Ordering::SeqCst) + self.conn.controls
    }

    /// How many more packet ids may be handed out now without the oldest outstanding request's id
    /// coming round again; `None`, no request outstanding.
    fn ids_left(&self, ctx: &Ctx<'_>) -> Option<u64> {
        let oldest = self.conn.outstanding().map(|p| p.clock).min()?;
        let since = self.clock(ctx).saturating_sub(oldest);
        Some(u64::from(ctx.line.id_space()).saturating_sub(1).saturating_sub(since))
    }

    /// Hand rumqttc the waiting requests that may go — the line clear of publishes, and ids left
    /// (see [`Conn`]) — then `Active` if it is due, then set the publication's budget. Called after
    /// every event the lifecycle handles.
    async fn drive(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) -> Result<(), Failed> {
        self.conn.line_wait = false;
        while self.conn.connected && !self.conn.waiting.is_empty() {
            if !ctx.line_clear() {
                self.conn.line_wait = true;
                break;
            }
            if self.ids_left(ctx) == Some(0) {
                break;
            }
            let Some(control) = self.conn.waiting.pop_front() else {
                break;
            };
            self.conn.controls += 1;
            let pending = Pending {
                purpose: control.purpose,
                unsubscribe: matches!(control.request, Request::Unsubscribe(_)),
                deadline: Instant::now() + GRANT_BOUND,
                clock: self.clock(ctx),
            };
            self.conn.unnumbered.push_back(pending);
            match control.request {
                // WAIT: mqtt-request
                Request::Subscribe(filters) => ctx.client.subscribe_many(filters).await?,
                // WAIT: mqtt-request
                Request::Unsubscribe(filter) => ctx.client.unsubscribe(filter).await?,
            }
        }
        self.maybe_active(ctx, outputs).await;
        outputs.set_budget(self.budget(ctx));
        Ok(())
    }

    /// How many more messages the session may publish now (`None`: no limit): none while a request
    /// waits for the line to clear of publishes; with requests outstanding, the ids left.
    fn budget(&self, ctx: &Ctx<'_>) -> Option<usize> {
        if !self.conn.connected {
            return None;
        }
        if self.conn.line_wait {
            return Some(0);
        }
        self.ids_left(ctx).map(|left| usize::try_from(left).unwrap_or(usize::MAX))
    }

    /// The next request waits for the line to clear of publishes: the session wakes on the pump's
    /// progress.
    pub(crate) fn line_wait(&self) -> bool {
        self.conn.line_wait
    }

    /// The pump has progressed while a request waited for the line.
    pub(crate) async fn on_line(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) -> Result<Flow, Failed> {
        self.drive(ctx, outputs).await.map(|()| Flow::Continue)
    }

    /// How many more messages the shutdown's flush may publish now (`None`: no limit): with a
    /// request outstanding, the ids left before its packet id would come round (`Conn`). Nothing is
    /// handed to rumqttc at the flush, so no request waits for the line.
    pub(crate) fn flush_budget(&self, ctx: &Ctx<'_>) -> Option<usize> {
        if !self.conn.connected {
            return None;
        }
        self.ids_left(ctx).map(|left| usize::try_from(left).unwrap_or(usize::MAX))
    }

    /// A request's packet id seen written, or its acknowledgement: the request it is for, taken
    /// out once acknowledged (`None` for an id seen, or an acknowledgement of nothing awaited).
    fn number(&mut self, event: &PumpEvent) -> Option<Pending> {
        match *event {
            PumpEvent::Subscribing(pkid) | PumpEvent::Unsubscribing(pkid) => {
                match self.conn.unnumbered.pop_front() {
                    Some(pending) => {
                        if self.conn.numbered.insert(pkid, pending).is_some() {
                            warn!(pkid, "rumqttc handed out a packet id still awaiting its acknowledgement");
                        }
                    }
                    None => debug!(pkid, "A subscription request the lifecycle did not make"),
                }
                None
            }
            PumpEvent::SubAck { pkid, .. } | PumpEvent::UnsubAck { pkid, .. } => {
                let pending = self.conn.numbered.remove(&pkid);
                if pending.is_none() {
                    debug!(pkid, "An acknowledgement of nothing awaited");
                }
                pending
            }
            _ => None,
        }
    }

    /// The bookkeeping of a request's packet id or acknowledgement alone, nothing acted on: the
    /// shutdown's flush, waiting for those outstanding.
    pub(crate) fn settle_ack(&mut self, event: &PumpEvent) {
        let _ = self.number(event);
    }

    fn filter(path: String, retain_as_published: bool) -> Filter {
        Filter { preserve_retain: retain_as_published, ..Filter::new(path, QoS::AtLeastOnce) }
    }

    /// S1, the read-back: the instance's targets' `State` and its `Status`, kept while it is owned.
    /// The fixed instance's is R1's: its own `State/{i}/#` alone, until its replay is in.
    async fn request_read(&mut self, ctx: Option<&Ctx<'_>>, name: &str) -> Result<(), Failed> {
        let Some((id, topics)) = self.rt_of(name).map(|(rt, topics)| (rt.id, topics.clone())) else {
            return Ok(());
        };
        if !self.conn.connected {
            return Ok(());
        }
        self.set_sub(name, |rt| rt.read = Sub::Requested);
        let filters = match self.fixed.as_mut().filter(|f| f.name == name) {
            Some(fixed) => {
                fixed.read_back_until = Some(Instant::now() + session::READ_BACK_WAIT);
                vec![Self::filter(topics.state_filter(), false)]
            }
            None => vec![Self::filter(topics.state_target_filter(), false), Self::filter(topics.status(), false)],
        };
        self.request_subscription(ctx, Purpose::Read { instance: name.to_owned(), rt: id }, filters).await
    }

    /// The fixed instance's requests: R1's four SUBSCRIBEs, in R1's order. Configs first, so a
    /// target's retained setup is queued ahead of its retained values (review M4); target-level
    /// writes and commands retain-as-published (never retained: a live retained one is refused),
    /// property writes without (only a replay arrives flagged; review C10).
    async fn request_fixed_lanes(&mut self, ctx: Option<&Ctx<'_>>, name: &str) -> Result<(), Failed> {
        let Some((id, topics)) = self.rt_of(name).map(|(rt, topics)| (rt.id, topics.clone())) else {
            return Ok(());
        };
        if !self.conn.connected {
            return Ok(());
        }
        self.set_sub(name, |rt| {
            rt.setup = Sub::Requested;
            rt.requests = Sub::Requested;
        });
        let instance = name.to_owned();
        let setup = Purpose::Setup { instance: instance.clone(), rt: id };
        self.request_subscription(ctx, setup, vec![Self::filter(topics.config_filter(), false)]).await?;
        let requests = [(topics.desired_target_filter(), true), (topics.desired_property_filter(), false), (topics.command(), true)];
        for (filter, retain_as_published) in requests {
            let purpose = Purpose::Requests { instance: instance.clone(), rt: id };
            self.request_subscription(ctx, purpose, vec![Self::filter(filter, retain_as_published)]).await?;
        }
        Ok(())
    }

    /// S2, the target Configs.
    async fn request_setup(&mut self, ctx: Option<&Ctx<'_>>, name: &str) -> Result<(), Failed> {
        let Some((id, topics)) = self.rt_of(name).map(|(rt, topics)| (rt.id, topics.clone())) else {
            return Ok(());
        };
        if !self.conn.connected {
            return Ok(());
        }
        self.set_sub(name, |rt| rt.setup = Sub::Requested);
        if let Some(instance) = self.instances.get_mut(name) {
            instance.replayed.clear();
        }
        let filters = vec![Self::filter(topics.config_target_filter(), false)];
        self.request_subscription(ctx, Purpose::Setup { instance: name.to_owned(), rt: id }, filters).await
    }

    /// S3, the requests: property writes without retain-as-published (only a replay arrives
    /// flagged), target-level writes and commands with it (never retained: a live retained one is
    /// refused).
    async fn request_requests(&mut self, ctx: Option<&Ctx<'_>>, name: &str) -> Result<(), Failed> {
        let Some((id, topics)) = self.rt_of(name).map(|(rt, topics)| (rt.id, topics.clone())) else {
            return Ok(());
        };
        if !self.conn.connected {
            return Ok(());
        }
        self.set_sub(name, |rt| rt.requests = Sub::Requested);
        let filters = vec![
            Self::filter(topics.desired_property_filter(), false),
            Self::filter(topics.desired_target_filter(), true),
            Self::filter(topics.command(), true),
        ];
        self.request_subscription(ctx, Purpose::Requests { instance: name.to_owned(), rt: id }, filters).await
    }

    fn rt_of(&self, name: &str) -> Option<(&Rt, &Topics)> {
        self.instances.get(name).and_then(|i| i.rt.as_ref().map(|rt| (rt, &i.topics)))
    }

    fn set_sub(&mut self, name: &str, set: impl FnOnce(&mut Rt)) {
        if let Some(rt) = self.instances.get_mut(name).and_then(|i| i.rt.as_mut()) {
            set(rt);
        }
    }

    // ---- The session's events --------------------------------------------------------------

    /// A CONNACK: the root subscriptions — `Config/+`, then the barrier — and every owned
    /// instance's bootstrap again (the broker keeps no session).
    pub(crate) async fn on_connack(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) -> Result<(), Failed> {
        self.conn = Conn { connected: true, ..Conn::default() };
        outputs.outbox_open = true;
        // A fixed host (N = 1) owns no instance by Config: no `Config/+`, no barrier, no
        // `Status/+`. Its instance's read-back, once per process, then its requests (R1).
        if let Some(fixed) = &self.fixed {
            let (name, read_back) = (fixed.name.clone(), !fixed.read_back_done);
            if read_back {
                self.request_read(Some(ctx), &name).await?;
            } else {
                self.request_fixed_lanes(Some(ctx), &name).await?;
            }
            return self.drive(ctx, outputs).await;
        }
        let root = self.settings.root.clone();
        let configs = Self::filter(format!("{root}/Config/+"), false);
        let barrier = Self::filter(format!("{root}/_barrier/{}", self.settings.id), false);
        self.request_subscription(Some(ctx), Purpose::Configs, vec![configs]).await?;
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            let phase = self.instances.get(&name).and_then(|i| i.rt.as_ref()).map(|rt| rt.phase);
            // A stop that ends disabled keeps S1; one that ends deleted reads its slot afresh for
            // its cleanup; one for a successor says `connecting` before `Active=true` (§4.6; R2
            // gate, Claude minor 4) — the successor then bootstraps its own.
            let read = matches!(
                phase,
                Some(
                    Phase::Live
                        | Phase::Disabled
                        | Phase::Stopping { then: Then::Disable | Then::Release { cleanup: true } | Then::Successor, .. }
                )
            );
            if read {
                self.request_read(Some(ctx), &name).await?;
            }
        }
        // The barrier behind the instances already owned: their read-backs are asked for ahead of
        // it, so a late barrier SUBACK — holding the ids handed out after it (`Conn`) — delays
        // their bootstrap the least.
        // The process's first root replay also reads `Status/+`, closed by the same barrier: the
        // instances whose retained Status carries this process's tag though it does not own them
        // (deleted while it was down) are swept (§4.5; R2 gate, Claude 3).
        if matches!(self.scan, Scan::Pending) {
            self.scan = Scan::Reading(BTreeSet::new());
            let statuses = Self::filter(format!("{root}/Status/+"), false);
            self.request_subscription(Some(ctx), Purpose::Statuses, vec![statuses]).await?;
        }
        self.request_subscription(Some(ctx), Purpose::Barrier, vec![barrier]).await?;
        self.drive(ctx, outputs).await
    }

    /// A SUBSCRIBE or UNSUBSCRIBE written (its packet id), or its acknowledgement.
    pub(crate) async fn on_ack(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, event: PumpEvent) -> Result<Flow, Failed> {
        let flow = self.ack(ctx, outputs, event).await?;
        if flow == Flow::Continue {
            self.drive(ctx, outputs).await?;
        }
        Ok(flow)
    }

    async fn ack(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, event: PumpEvent) -> Result<Flow, Failed> {
        match event {
            PumpEvent::Subscribing(_) | PumpEvent::Unsubscribing(_) => {
                let _ = self.number(&event);
                Ok(Flow::Continue)
            }
            PumpEvent::SubAck { granted, .. } | PumpEvent::UnsubAck { granted, .. } => {
                let Some(pending) = self.number(&event) else {
                    return Ok(Flow::Continue);
                };
                if !granted && self.fixed_purpose(&pending.purpose) {
                    self.fixed_refused(ctx, outputs, pending.purpose).await?;
                    self.maybe_active(ctx, outputs).await;
                    return Ok(Flow::Continue);
                }
                if !granted {
                    warn!(
                        kind = "subscription_refused",
                        purpose = ?pending.purpose,
                        unsubscribe = pending.unsubscribe,
                        "The broker refused a subscription the host needs; ending the session"
                    );
                    return Ok(Flow::End);
                }
                if pending.unsubscribe {
                    self.unsubscribed(ctx, outputs, pending.purpose).await?;
                } else {
                    self.granted(ctx, outputs, pending.purpose).await?;
                }
                self.maybe_active(ctx, outputs).await;
                Ok(Flow::Continue)
            }
            _ => Ok(Flow::Continue),
        }
    }

    /// A refusal of one of the fixed instance's subscriptions, taken as R1 took it — the session
    /// goes on. The read-back's is no read-back: said, WARN `read_back_incomplete`, and the
    /// requests subscribed without it. A request's reason code is not read (R1 never read them):
    /// as a grant, so the next SUBACK still completes the read-back.
    async fn fixed_refused(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, purpose: Purpose) -> Result<(), Failed> {
        match purpose {
            Purpose::Read { instance, rt } => {
                if !self.fixed_read_back_awaited(&instance, rt) {
                    return Ok(());
                }
                if let Some(rt) = self.instances.get_mut(&instance).and_then(|i| i.rt.as_mut()) {
                    rt.read = Sub::None;
                    rt.device.reports.read_back_incomplete();
                }
                warn!(
                    kind = "read_back_incomplete",
                    refused = true,
                    "The broker refused the read-back's subscription; subscribing to requests without it — a replay may meet no \"before\" values"
                );
                self.request_fixed_lanes(Some(ctx), &instance).await
            }
            Purpose::Unsub { .. } => Ok(()),
            purpose => self.granted(ctx, outputs, purpose).await,
        }
    }

    /// Whether the fixed instance's read-back asked on this connection still waits for its SUBACK
    /// — not refused, granted or late yet. Its SUBACK, the first to come, settles it: once per
    /// process.
    fn fixed_read_back_awaited(&mut self, instance: &str, rt: u64) -> bool {
        let awaited = self.current(instance, rt).is_some_and(|rt| rt.read == Sub::Requested);
        if let Some(fixed) = self.fixed.as_mut().filter(|_| awaited) {
            fixed.read_back_done = true;
            fixed.read_back_until = None;
        }
        awaited
    }

    /// The fixed instance's read-back did not get its SUBACK within
    /// [`READ_BACK_WAIT`](session::READ_BACK_WAIT): done, as far as the conflict rule goes —
    /// unsubscribed, its own State coming back would only queue ahead of the requests (Codex
    /// final gate 1) — and the requests subscribed without it (R1). Its SUBACK, should it still
    /// come, completes nothing; it is awaited, as every request is, within [`GRANT_BOUND`].
    async fn fixed_read_back_late(&mut self, ctx: &Ctx<'_>) -> Result<(), Failed> {
        let Some(name) = self.fixed_name() else {
            return Ok(());
        };
        let Some((id, topics)) = self.rt_of(&name).map(|(rt, topics)| (rt.id, topics.clone())) else {
            return Ok(());
        };
        if !self.fixed_read_back_awaited(&name, id) {
            return Ok(());
        }
        if let Some(rt) = self.instances.get_mut(&name).and_then(|i| i.rt.as_mut()) {
            rt.read = Sub::None;
            rt.device.reports.read_back_incomplete();
        }
        self.request_unsubscription(Some(ctx), Purpose::Unsub { instance: name.clone(), rt: id }, topics.state_filter()).await?;
        warn!(
            kind = "read_back_incomplete",
            waited_ms = session::READ_BACK_WAIT.as_millis() as u64,
            "The broker did not acknowledge the read-back's subscription in time; subscribing to requests without it — a replay may meet no \"before\" values"
        );
        self.request_fixed_lanes(Some(ctx), &name).await
    }

    async fn granted(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, purpose: Purpose) -> Result<(), Failed> {
        match purpose {
            Purpose::Configs => self.conn.configs_granted = Some(Instant::now()),
            Purpose::Barrier => {
                self.conn.barrier_acked = true;
                self.replay_complete(Some(ctx), outputs).await?;
                if let Scan::Reading(own) = std::mem::replace(&mut self.scan, Scan::Done) {
                    let litter: Vec<String> = own.into_iter().filter(|name| !self.instances.contains_key(name)).collect();
                    if !litter.is_empty() {
                        info!(instances = litter.len(), "Retained Status tagged with this process for instances it does not own: swept");
                    }
                    self.cleanups.extend(litter);
                    let statuses = format!("{}/Status/+", self.settings.root);
                    self.request_unsubscription(Some(ctx), Purpose::StatusesDone, statuses).await?;
                }
                self.hand_over_cleanups(ctx).await?;
            }
            Purpose::Read { instance, rt } if self.is_fixed(&instance) => {
                // The fixed instance's read-back is granted: its retained State follows, and the
                // next SUBACK — its requests' first, asked now — comes behind it (R1).
                if self.fixed_read_back_awaited(&instance, rt) {
                    self.set_sub(&instance, |rt| rt.read = Sub::Granted);
                    self.request_fixed_lanes(Some(ctx), &instance).await?;
                }
            }
            Purpose::Read { instance, rt } => {
                let Some(device) = self.current(&instance, rt).map(|rt| rt.device.clone()) else {
                    return Ok(());
                };
                self.set_sub(&instance, |rt| rt.read = Sub::Granted);
                // Its slot is read: its output may go out, its current Status and State first —
                // before `Active=true` (multi-device spec §4.6). Not a stopping runtime's: its slot
                // is no longer its to write. Through the rotation, within the packet-id budget
                // (§4.3): a direct publish could take the id of a SUBSCRIBE still awaiting its
                // SUBACK — another instance's read-back, or the barrier (the R2 confirming
                // reviews).
                if self.current(&instance, rt).is_some_and(|rt| matches!(rt.phase, Phase::Live | Phase::Disabled)) {
                    if let Some(slot) = outputs.slot(rt) {
                        slot.batch.add_model(&device, device.reports.model(), true);
                    }
                    outputs.announce(rt);
                }
                // Stopping for a successor across a reconnect: the instance's lifecycle Status is
                // the successor's to come, `connecting` — said before `Active=true`, so the old
                // device's retained `connected` does not stand meanwhile (§4.6; R2 gate, Claude
                // minor 4). Through the outbox, within the budget, as above.
                if self.current(&instance, rt).is_some_and(|rt| matches!(rt.phase, Phase::Stopping { then: Then::Successor, .. })) {
                    let status = LinkStatus::Connecting.as_str().as_bytes().to_vec();
                    let out = Out::new(device.topics.status(), true, status, None, Some(&self.settings.id)).first();
                    outputs.queue_outbox(Part::Model, out);
                }
                if self.current(&instance, rt).is_some_and(|rt| rt.phase == Phase::Live) {
                    self.request_setup(Some(ctx), &instance).await?;
                }
            }
            Purpose::Setup { instance, rt } => {
                let Some(device) = self.current(&instance, rt).map(|rt| rt.device.clone()) else {
                    return Ok(());
                };
                self.set_sub(&instance, |rt| rt.setup = Sub::Granted);
                if self.is_fixed(&instance) {
                    // The fixed instance's requests' first SUBACK, behind the read-back's replay,
                    // completes it; then its subscription goes — kept, it would only bring the
                    // bridge's own State back, a publish for each one it makes, ahead of every
                    // request (Codex final gate 1).
                    if self.current(&instance, rt).is_some_and(|rt| rt.read == Sub::Granted) {
                        device.reports.read_back_complete();
                        self.set_sub(&instance, |rt| rt.read = Sub::None);
                        let unsub = Purpose::Unsub { instance: instance.clone(), rt };
                        self.request_unsubscription(Some(ctx), unsub, device.topics.state_filter()).await?;
                    }
                    return Ok(());
                }
                // S2's SUBACK closes S1's replay: the conflict snapshot freezes.
                device.reports.read_back_complete();
                if self.current(&instance, rt).is_some_and(|rt| rt.phase == Phase::Live) {
                    self.request_requests(Some(ctx), &instance).await?;
                }
            }
            Purpose::Requests { instance, rt } => {
                // The fixed instance serves from the start: nothing waits for its requests' grants.
                if self.current(&instance, rt).is_none() || self.is_fixed(&instance) {
                    return Ok(());
                }
                // S3's SUBACK closes S2's replay: a target Config no longer retained is retracted
                // (on a later bootstrap); then the device may execute.
                if let Some(inst) = self.instances.get_mut(&instance) {
                    if let Some(rt) = inst.rt.as_mut() {
                        rt.requests = Sub::Granted;
                        if rt.bootstrapped {
                            let gone: Vec<String> = inst.installed.difference(&inst.replayed).cloned().collect();
                            for target in gone {
                                info!(instance = %inst.name, target = %target, "A target Config no longer retained; retracted");
                                rt.device.mailbox.post_config(&target, None);
                                inst.installed.remove(&target);
                            }
                        }
                        rt.bootstrapped = true;
                    }
                }
                self.start_if_ready(Some(ctx), outputs, &instance).await?;
            }
            Purpose::Sweep { instance } => {
                // Owned again meanwhile: its own read-back needs these filters, and heals its slot.
                if self.instances.contains_key(&instance) {
                    self.sweeps.remove(&instance);
                    return Ok(());
                }
                let Ok(topics) = Topics::new(&self.settings.root, &instance) else {
                    return Ok(());
                };
                let mut closing = 0;
                for filter in [topics.state_target_filter(), topics.status()] {
                    if self.request_unsubscription(Some(ctx), Purpose::SweepDone { instance: instance.clone() }, filter).await? {
                        closing += 1;
                    }
                }
                if let Some(sweep) = self.sweeps.get_mut(&instance) {
                    sweep.closing = closing;
                }
            }
            Purpose::Unsub { .. } | Purpose::Statuses | Purpose::StatusesDone | Purpose::SweepDone { .. } => {}
        }
        Ok(())
    }

    async fn unsubscribed(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, purpose: Purpose) -> Result<(), Failed> {
        if let Purpose::SweepDone { instance } = &purpose {
            let done = self.sweeps.get_mut(instance).is_some_and(|sweep| {
                sweep.closing = sweep.closing.saturating_sub(1);
                sweep.closing == 0
            });
            if done {
                self.sweep_done(outputs, instance);
            }
            return Ok(());
        }
        if let Purpose::Unsub { instance, rt } = purpose {
            let done = match self.instances.get_mut(&instance).and_then(|i| i.rt.as_mut()).filter(|r| r.id == rt) {
                Some(rt) if matches!(rt.phase, Phase::Stopping { step: Step::Unsubscribe, .. }) => {
                    rt.unsubscribing = rt.unsubscribing.saturating_sub(1);
                    rt.unsubscribing == 0
                }
                _ => false,
            };
            if done {
                self.finish_stop(Some(ctx), outputs, &instance).await?;
            }
        }
        Ok(())
    }

    fn current(&self, name: &str, id: u64) -> Option<&Rt> {
        self.instances.get(name).and_then(|i| i.rt.as_ref()).filter(|rt| rt.id == id)
    }

    /// `Active=true` (and the `Version`) once every subscription requested so far is granted and
    /// the root replay is closed by the barrier — or [`REPLAY_FALLBACK`] has passed since the
    /// `Config/+` grant — and every owned instance's current Status and State said at its
    /// read-back's grant has gone out (multi-device spec §4.6). A pending grant is never bypassed.
    ///
    /// The fixed instance's `Active=true` is R1's: once its requests are subscribed — handed over,
    /// not granted — and before its model, which follows `Active` and the `Version` through the
    /// rotation (Bridge Runtime spec §9: nothing published before the subscriptions, the first
    /// publish `Active=true`). No barrier: a fixed host owns nothing by Config.
    async fn maybe_active(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) {
        // `Active` and `Version` within the publication's budget (`Conn`): a request outstanding
        // has ids to spare for them — the barrier's, on the fallback — or its acknowledgement
        // checks again. The statuses before it: they go through the rotation, and the turn that
        // publishes the last of them checks again (`on_turn_done`).
        if self.conn.active || !self.conn.connected || self.budget(ctx).is_some_and(|b| b < 2) {
            return;
        }
        let fixed = self.fixed_name().and_then(|name| self.rt_of(&name).map(|(rt, _)| (rt.id, rt.requests, rt.device.clone())));
        match &fixed {
            Some((_, requests, _)) => {
                if *requests == Sub::None || self.conn.waiting.iter().any(|c| !c.is_unsubscribe()) {
                    return;
                }
            }
            None => {
                if self.conn.pending_subscriptions() > 0 || outputs.firsts_waiting() {
                    return;
                }
                let Some(granted) = self.conn.configs_granted else {
                    return;
                };
                if !self.conn.barrier_acked && Instant::now() < granted + REPLAY_FALLBACK {
                    return;
                }
                if !self.conn.barrier_acked {
                    info!("The root replay's barrier is not acknowledged yet; Active=true goes without it — releasing nothing until it is");
                }
            }
        }
        self.conn.active = true;
        let shared = ctx.shared;
        session::emit(shared, ctx.client, shared.topics.active(), true, b"true".to_vec(), session::properties()).await;
        session::emit(shared, ctx.client, shared.topics.version(), true, shared.version.clone().into_bytes(), session::properties()).await;
        // The fixed instance's whole retained model again, from its own state — a broker restarted
        // without persistence lost it (the fleet's F4) — then its reports from now on.
        if let Some((id, _, device)) = fixed {
            if let Some(slot) = outputs.slot(id) {
                slot.batch.add_model(&device, device.reports.model(), false);
            }
            outputs.announce(id);
        }
    }

    /// The root replay is closed by the barrier's SUBACK (multi-device spec §4.3, reconnect
    /// reconciliation): every owned instance whose Config did not come is released as deleted.
    async fn replay_complete(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs) -> Result<(), Failed> {
        let absent: Vec<String> = self.instances.keys().filter(|name| !self.conn.replayed.contains(*name)).cloned().collect();
        for name in absent {
            if let Some(instance) = self.instances.get(&name) {
                if matches!(instance.want, Want::Leave { .. }) {
                    continue;
                }
            }
            info!(instance = %name, "An owned instance's Config was not in the replay: released as deleted");
            self.set_want(&name, Want::Leave { cleanup: true });
            self.reconcile(ctx, outputs, &name).await?;
        }
        Ok(())
    }

    /// An incoming publish (multi-device spec §4.3 routing; "Everything else on the root is ignored
    /// (DEBUG)").
    pub(crate) async fn on_publish(
        &mut self,
        ctx: &Ctx<'_>,
        outputs: &mut Outputs,
        topic: &str,
        publish: &Publish,
        received: Instant,
    ) -> Result<Flow, Failed> {
        // The fixed instance takes every publish, as R1's one device did: its own grammar answers
        // what is none of its lanes (on `Error`); no Config, slot or tag is read.
        if let Some(device) = self.fixed_name().and_then(|name| self.rt_of(&name).map(|(rt, _)| rt.device.clone())) {
            session::route(&device, topic, publish, received);
            self.drive(ctx, outputs).await?;
            return Ok(Flow::Continue);
        }
        let Some(rest) = topic.strip_prefix(self.settings.root.as_str()).and_then(|r| r.strip_prefix('/')) else {
            debug!(topic, "A publish off the root; ignored");
            return Ok(Flow::Continue);
        };
        let segments: Vec<&str> = rest.split('/').collect();
        // The first root replay's `Status/+`: an instance whose retained Status carries the own tag.
        if let (Scan::Reading(own), ["Status", instance]) = (&mut self.scan, segments.as_slice()) {
            if publish.retain && !publish.payload.is_empty() && single_via(publish).as_deref() == Some(self.settings.id.as_str()) {
                own.insert((*instance).to_owned());
            }
        }
        match segments.as_slice() {
            ["Config", instance] => self.config(Some(ctx), outputs, instance, publish).await?,
            ["_barrier", ..] => {}
            [_, instance, ..] if self.instances.contains_key(*instance) => self.route_instance(outputs, instance, topic, publish, received),
            // A cleanup's read-back of an instance this process does not own.
            ["State" | "Status", instance, ..] if self.sweeps.contains_key(*instance) => {
                if let Some(sweep) = self.sweeps.get_mut(*instance) {
                    if publish.payload.is_empty() {
                        sweep.seen.remove(topic);
                    } else {
                        sweep.seen.insert(topic.to_owned(), single_via(publish));
                    }
                }
            }
            _ => debug!(topic, "A publish for no instance this process owns; ignored"),
        }
        self.drive(ctx, outputs).await?;
        Ok(Flow::Continue)
    }

    /// A device's keeper or a construction's watcher, during a session.
    pub(crate) async fn on_event(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, event: Event) -> Result<Flow, Failed> {
        self.handle_event(Some(ctx), outputs, event).await?;
        self.drive(ctx, outputs).await?;
        Ok(Flow::Continue)
    }

    /// The same between sessions: nothing can be subscribed — nothing here waits — and the next
    /// CONNACK bootstraps.
    pub(crate) async fn on_event_offline(&mut self, outputs: &mut Outputs, event: Event) {
        let _ = self.handle_event(None, outputs, event).await;
    }

    async fn handle_event(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, event: Event) -> Result<(), Failed> {
        match event {
            Event::Stopped { id } => {
                let name = self
                    .instances
                    .values()
                    .find(|i| i.rt.as_ref().is_some_and(|rt| rt.id == id && matches!(rt.phase, Phase::Stopping { step: Step::Worker, .. })))
                    .map(|i| i.name.clone());
                if let Some(name) = name {
                    self.worker_stopped(ctx, outputs, &name).await?;
                }
            }
            Event::Built { instance, id, result } => self.built(ctx, outputs, &instance, id, result).await?,
        }
        Ok(())
    }

    /// A timer: the fixed instance's read-back, late; an overdue grant ends the session; the
    /// replay's fallback; a failed instance's retry; the sweep of failed instances' held requests.
    pub(crate) async fn on_timer(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) -> Result<Flow, Failed> {
        let now = Instant::now();
        if self.fixed.as_ref().and_then(|f| f.read_back_until).is_some_and(|until| until <= now) {
            self.fixed_read_back_late(ctx).await?;
        }
        if let Some(overdue) = self.conn.outstanding().find(|p| p.deadline <= now) {
            warn!(
                kind = "subscription_refused",
                purpose = ?overdue.purpose,
                unsubscribe = overdue.unsubscribe,
                overdue_ms = GRANT_BOUND.as_millis() as u64,
                "The broker did not acknowledge a subscription the host needs in time; ending the session"
            );
            return Ok(Flow::End);
        }
        self.maybe_active(ctx, outputs).await;
        let due: Vec<String> = self.instances.values().filter(|i| i.retry_at.is_some_and(|at| at <= now)).map(|i| i.name.clone()).collect();
        for name in due {
            if let Some(instance) = self.instances.get_mut(&name) {
                instance.retry_at = None;
            }
            self.reconcile(Some(ctx), outputs, &name).await?;
        }
        if self.next_sweep.is_some_and(|at| at <= now) {
            self.sweep();
        }
        self.drive(ctx, outputs).await?;
        Ok(Flow::Continue)
    }

    /// Failed instances hold their property requests with no worker to take them: those past their
    /// deadline are dropped ("settled by the session", multi-device spec §4.3).
    fn sweep(&mut self) {
        let mut any = false;
        for instance in self.instances.values().filter(|i| i.failed()) {
            any = true;
            if let Some(rt) = &instance.rt {
                let swept = rt.device.mailbox.sweep_expired();
                if swept > 0 {
                    debug!(instance = %instance.name, swept, "A failed instance's held requests dropped at their deadline");
                }
            }
        }
        self.next_sweep = any.then(|| Instant::now() + SWEEP_EVERY);
    }

    /// After a turn of the rotation: a stopping device whose output has retired goes on to its
    /// unsubscriptions.
    pub(crate) async fn on_turn_done(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) -> Result<Flow, Failed> {
        // The cleanups the outbox has handed over are done once the broker has acknowledged them.
        if outputs.outbox.is_empty() {
            let enqueued = ctx.shared.enqueued.load(std::sync::atomic::Ordering::SeqCst);
            for (_, handed) in self.queued_cleanups.iter_mut().filter(|(_, handed)| handed.is_none()) {
                *handed = Some(enqueued);
            }
        }
        let acked = ctx.line.acknowledged();
        self.queued_cleanups.retain(|(_, handed)| handed.map_or(true, |handed| handed > acked));
        let retiring: Vec<String> = self
            .instances
            .values()
            .filter(|i| i.rt.as_ref().is_some_and(|rt| matches!(rt.phase, Phase::Stopping { step: Step::Retire, .. })))
            .map(|i| i.name.clone())
            .collect();
        for name in retiring {
            self.check_retired(Some(ctx), outputs, &name).await?;
        }
        self.drive(ctx, outputs).await?;
        Ok(Flow::Continue)
    }

    /// The connection is lost: its subscriptions are gone with it. A stopping device's output not
    /// yet published is dropped (WARN `output_lost_at_session_end`), as R1 drops unpublished output
    /// at process shutdown; the next CONNACK bootstraps every owned instance again.
    pub(crate) async fn on_session_lost(&mut self, outputs: &mut Outputs) {
        self.conn = Conn::default();
        outputs.outbox_open = false;
        outputs.set_budget(None);
        // Cleanups not acknowledged on this connection, and the sweeps under way: pending again.
        let queued = std::mem::take(&mut self.queued_cleanups);
        self.cleanups.extend(queued.into_iter().map(|(name, _)| name));
        let sweeping = std::mem::take(&mut self.sweeps);
        self.cleanups.extend(sweeping.into_keys());
        if matches!(self.scan, Scan::Reading(_)) {
            self.scan = Scan::Pending;
        }
        // The fixed instance's read-back asked and not answered is asked again on the next
        // connection; one granted and not complete is incomplete (below), and done (R1).
        if let Some(fixed) = self.fixed.as_mut() {
            fixed.read_back_until = None;
        }
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            let Some(rt) = self.instances.get_mut(&name).and_then(|i| i.rt.as_mut()) else {
                continue;
            };
            // A read-back replay cut short is what there is.
            if rt.read == Sub::Granted && rt.setup != Sub::Granted {
                rt.device.reports.read_back_incomplete();
            }
            rt.read = Sub::None;
            rt.setup = Sub::None;
            rt.requests = Sub::None;
            rt.unsubscribing = 0;
            if matches!(rt.phase, Phase::Stopping { step: Step::Retire | Step::Unsubscribe, .. }) {
                let _ = self.check_retired(None, outputs, &name).await;
                if let Some(rt) = self.instances.get(&name).and_then(|i| i.rt.as_ref()) {
                    if matches!(rt.phase, Phase::Stopping { step: Step::Unsubscribe, .. }) {
                        let _ = self.finish_stop(None, outputs, &name).await;
                    }
                }
            }
        }
    }

    // ---- Config ----------------------------------------------------------------------------

    /// A `Config/{instance}` (multi-device spec §3).
    async fn config(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str, publish: &Publish) -> Result<(), Failed> {
        let payload = &publish.payload[..];
        if !payload.is_empty() {
            self.conn.replayed.insert(name.to_owned());
        }
        let owned = self.instances.contains_key(name);
        let (kind, id) = (self.settings.kind.clone(), self.settings.id.clone());
        match classify(payload, &kind, &id) {
            Verdict::Deleted => {
                self.rejected.remove(name);
                if owned {
                    info!(instance = name, "The instance was deleted");
                    self.set_want(name, Want::Leave { cleanup: true });
                    self.reconcile(ctx, outputs, name).await?;
                }
            }
            Verdict::Unreadable(error) => match self.instances.get_mut(name) {
                Some(instance) if instance.invalid.as_deref() != Some(payload) => {
                    instance.invalid = Some(payload.to_vec());
                    warn!(kind = "device_config_invalid", instance = name, error = %error, "An instance's Config cannot be read; its last accepted Config stays in force");
                }
                Some(_) => {}
                None => debug!(instance = name, error = %error, "A Config naming no process this one can tell; ignored"),
            },
            Verdict::Foreign(owner) => {
                self.rejected.remove(name);
                if owned {
                    info!(instance = name, owner = %owner, "The instance's Config names another process; released");
                    self.set_want(name, Want::Leave { cleanup: false });
                    self.reconcile(ctx, outputs, name).await?;
                } else {
                    debug!(instance = name, owner = %owner, "Another process's instance; ignored");
                }
            }
            Verdict::KindMismatch { kind: named, .. } => {
                // Said once per Config: a replay of the same one is a no-op.
                if self.rejected.get(name).is_some_and(|p| p == payload) {
                    return Ok(());
                }
                self.rejected.insert(name.to_owned(), payload.to_vec());
                let error = format!("kind mismatch: the Config names `{named}`, this process is `{kind}`");
                self.config_rejected(outputs, name, Reason::Malformed, &error);
                // Not served: released, if this process served it — without the delete's cleanup:
                // §4.5 clears an owner's tagged topics on a delete or a reconciled absence only (R2
                // gate, Codex minor).
                if owned {
                    self.set_want(name, Want::Leave { cleanup: false });
                    self.reconcile(ctx, outputs, name).await?;
                }
            }
            Verdict::Malformed { config, error } => {
                if self.instances.get(name).is_some_and(|i| i.last_seen.as_ref() == Some(&config)) {
                    return Ok(());
                }
                self.own(outputs, name);
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.last_seen = Some(config);
                    // The last accepted Config stays in force; none — the instance is failed.
                    if !owned {
                        instance.want = Want::Broken;
                    }
                }
                self.config_rejected(outputs, name, Reason::Malformed, &error);
                self.reconcile(ctx, outputs, name).await?;
                if !owned {
                    self.mark_failed(name);
                }
            }
            Verdict::Ours { config, disabled, .. } => {
                if self.instances.get(name).is_some_and(|i| i.last_seen.as_ref() == Some(&config)) {
                    return Ok(());
                }
                self.rejected.remove(name);
                self.own(outputs, name);
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.last_seen = Some(config.clone());
                    instance.invalid = None;
                }
                self.set_want(name, if disabled { Want::Disable(config) } else { Want::Serve(config) });
                self.reconcile(ctx, outputs, name).await?;
            }
        }
        Ok(())
    }

    /// The root replay is complete: each pending cleanup of an instance that is not this process's
    /// again is swept — its slot read afresh (`Sweep`), so only what still carries the own tag is
    /// retracted (§4.5; R2 gate, Codex 7: the topics cached at the delete could be another owner's
    /// by now).
    async fn hand_over_cleanups(&mut self, ctx: &Ctx<'_>) -> Result<(), Failed> {
        for name in std::mem::take(&mut self.cleanups) {
            if self.instances.contains_key(&name) || self.sweeps.contains_key(&name) {
                continue;
            }
            let Ok(topics) = Topics::new(&self.settings.root, &name) else {
                continue;
            };
            debug!(instance = %name, "A deleted instance's slot read again for its cleanup");
            self.sweeps.insert(name.clone(), Sweep::default());
            let filters = vec![Self::filter(topics.state_target_filter(), false), Self::filter(topics.status(), false)];
            self.request_subscription(Some(ctx), Purpose::Sweep { instance: name }, filters).await?;
        }
        Ok(())
    }

    /// A sweep's replay is closed: what still carries the own tag is retracted, unless the instance
    /// is this process's again (its own healing then repairs its slot).
    fn sweep_done(&mut self, outputs: &mut Outputs, name: &str) {
        let Some(sweep) = self.sweeps.remove(name) else {
            return;
        };
        if self.instances.contains_key(name) {
            return;
        }
        let id = self.settings.id.clone();
        let own: Vec<String> = sweep.seen.into_iter().filter(|(_, via)| via.as_deref() == Some(id.as_str())).map(|(t, _)| t).collect();
        if own.is_empty() {
            debug!(instance = %name, "A deleted instance's slot holds nothing tagged with this process");
            return;
        }
        info!(instance = %name, topics = own.len(), "Clearing a deleted instance's retained topics this process tagged");
        for topic in own {
            outputs.queue_outbox(Part::Model, Out::new(topic, true, Vec::new(), None, Some(&id)));
        }
        self.queued_cleanups.push((name.to_owned(), None));
    }

    /// A Config naming this process was rejected: one `Error/{instance}` without a target, WARN
    /// `device_config_rejected`.
    fn config_rejected(&mut self, outputs: &mut Outputs, name: &str, reason: Reason, error: &str) {
        warn!(kind = "device_config_rejected", instance = name, reason = reason.as_str(), error, "An instance's Config was rejected");
        let report = ErrorReport::new(reason, error);
        match self.instances.get(name).and_then(|i| i.rt.as_ref()) {
            Some(rt) => rt.device.reports.request_error(report),
            None => {
                let topic = Topics::new(&self.settings.root, name).map(|t| t.error());
                if let Ok(topic) = topic {
                    outputs.queue_outbox(Part::Answers, Out::new(topic, false, report.encode(), None, None));
                }
            }
        }
    }

    /// Own `name`, if this process does not yet.
    fn own(&mut self, outputs: &mut Outputs, name: &str) {
        if self.instances.contains_key(name) {
            return;
        }
        let Ok(topics) = Topics::new(&self.settings.root, name) else {
            return;
        };
        info!(instance = name, "The instance's Config names this process; acquired");
        // Its own healing covers a cleanup still pending from an earlier delete — its retractions
        // still in the outbox included: sent after the new runtime's read-back is subscribed,
        // its replay would read the old own-tagged value as a "before" value and see it cleared
        // just after (the R2 confirming reviews, Claude minor 3). Dropped here, the outcome is the
        // one of an instance owned again before its sweep's replay closed (`sweep_done`).
        // Retractions already handed to rumqttc are acknowledged before the read-back is
        // subscribed: a SUBSCRIBE waits for the line to clear (`Conn`).
        self.cleanups.remove(name);
        self.queued_cleanups.retain(|(queued, _)| queued != name);
        let (status, prefix) = (topics.status(), format!("{}/", topics.state_target_filter().trim_end_matches("/+")));
        let dropped = outputs.outbox.discard(Part::Model, |out| out.retracts(&status, &prefix));
        if dropped > 0 {
            debug!(instance = name, dropped, "A pending cleanup's retractions dropped: the instance is this process's again");
        }
        self.instances.insert(name.to_owned(), Instance::new(name, topics, Want::Broken));
    }

    /// What the instance's latest Config asks. A stop under way not yet unsubscribing ends as the
    /// new want needs: released, or disabled.
    ///
    /// A disable whose driver fields still wait for their validation (§3), with a committed driver
    /// waiting to start — a replacement under way, its old device stopping for it, or its
    /// bootstrap not done — changes nothing yet: not the stop's destination, and not that driver.
    /// If the validation fails, the committed Config stays in force (§4.3); once validated, the
    /// disable is set again (`built`) and takes over (the R2 confirming reviews, Codex 2).
    fn set_want(&mut self, name: &str, want: Want) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        if instance.ready.is_some() && matches!(&want, Want::Disable(config) if Self::needs_validation(instance, config)) {
            instance.want = want;
            return;
        }
        if let Some(rt) = instance.rt.as_mut() {
            if let Phase::Stopping { step, then } = rt.phase {
                if step != Step::Unsubscribe {
                    let then = match &want {
                        Want::Leave { cleanup } => Then::Release { cleanup: *cleanup || matches!(then, Then::Release { cleanup: true }) },
                        Want::Disable(_) => Then::Disable,
                        _ => then,
                    };
                    rt.phase = Phase::Stopping { step, then };
                }
            }
        }
        if !matches!(want, Want::Serve(_)) {
            if let Some(ready) = instance.ready.take() {
                drop_off_thread(ready.driver);
            }
        }
        if matches!(want, Want::Leave { .. }) {
            instance.last_seen = None;
            instance.build = None;
            instance.retry_at = None;
        }
        instance.want = want;
    }

    /// Bring the instance towards what its Config asks.
    async fn reconcile(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let Some(instance) = self.instances.get(name) else {
            return Ok(());
        };
        if instance.rt.as_ref().is_some_and(|rt| matches!(rt.phase, Phase::Stopping { .. })) {
            // A disable during a stop is validated now, the stop going on as it was (`set_want`).
            if let Want::Disable(config) = &instance.want {
                if Self::needs_validation(instance, config) && instance.failed_config.as_ref() != Some(config) {
                    let config = config.clone();
                    self.ensure_build(name, config);
                }
            }
            return Ok(());
        }
        let phase = instance.rt.as_ref().map(|rt| (rt.phase, rt.worker));
        match instance.want.clone() {
            // Never configured: nothing to bring it towards.
            Want::Fixed => {}
            Want::Leave { cleanup } => match phase {
                Some(_) => self.begin_stop(ctx, outputs, name, Then::Release { cleanup }).await?,
                None => {
                    self.instances.remove(name);
                }
            },
            Want::Broken => {
                if phase.is_none() {
                    self.create_rt(ctx, outputs, name, Phase::Live, LinkStatus::Unreachable).await?;
                }
            }
            Want::Disable(config) if Self::needs_validation(instance, &config) => {
                // Its driver fields changed from the last accepted Config's: validated by the
                // factory before the disable is accepted — invalid, the last accepted Config stays
                // in force and the instance is not disabled (§3; R2 gate, Codex minor).
                if instance.failed_config.as_ref() != Some(&config) {
                    self.ensure_build(name, config);
                }
            }
            Want::Disable(config) => match phase {
                None => {
                    self.create_rt(ctx, outputs, name, Phase::Disabled, LinkStatus::Disabled).await?;
                    self.accept(name, config);
                }
                Some((Phase::Live, _)) => {
                    self.accept(name, config);
                    self.begin_stop(ctx, outputs, name, Then::Disable).await?;
                }
                Some(_) => self.accept(name, config),
            },
            Want::Serve(config) => {
                match phase {
                    None => self.create_rt(ctx, outputs, name, Phase::Live, LinkStatus::Connecting).await?,
                    // Enabled again: it leaves Disabled before anything is built — a live runtime
                    // with no driver yet, bootstrapping S2 and S3, `connecting` — so a failed
                    // construction takes the ordinary failed path (R2 gate, Claude 1).
                    Some((Phase::Disabled, _)) => self.re_enable(ctx, outputs, name).await?,
                    Some(_) => {}
                }
                let Some(instance) = self.instances.get_mut(name) else {
                    return Ok(());
                };
                let worker = instance.rt.as_ref().is_some_and(|rt| rt.worker);
                if worker && instance.accepted.as_ref() == Some(&config) {
                    // Serving it already: a construction under way for another Config is stale.
                    instance.build = None;
                    return Ok(());
                }
                if instance.ready.as_ref().is_some_and(|r| r.config == config) {
                    return self.commit(ctx, outputs, name).await;
                }
                // A driver ready for another Config — the one committed last, waiting for its
                // bootstrap — is kept until the wanted Config's construction succeeds: if it fails,
                // the last accepted Config stays in force (§3; R2 gate, Codex 4 / Claude 4).
                // A Config whose construction failed is built again only when nothing serves, at
                // the reconnect pacing.
                if instance.failed_config.as_ref() == Some(&config) && (worker || instance.ready.is_some() || instance.retry_at.is_some()) {
                    return Ok(());
                }
                self.ensure_build(name, config);
            }
        }
        Ok(())
    }

    /// Whether a disabled Config waits for its driver fields' validation: they differ from the last
    /// accepted Config's, and the factory has not accepted them yet. With no accepted Config there
    /// is nothing to keep in force: the fields are checked when the instance is enabled.
    fn needs_validation(instance: &Instance, config: &Value) -> bool {
        instance.accepted.as_ref().is_some_and(|accepted| driver_fields(accepted) != driver_fields(config))
            && instance.validated.as_ref() != Some(config)
    }

    fn accept(&mut self, name: &str, config: Value) {
        if let Some(instance) = self.instances.get_mut(name) {
            instance.accepted = Some(config);
            instance.failed_config = None;
        }
    }

    /// Start a construction for `config`, unless one is under way for it.
    fn ensure_build(&mut self, name: &str, config: Value) {
        let id = self.next();
        // A fixed host has no factory, and no Config to build from.
        let (Some(instance), Some(factory)) = (self.instances.get_mut(name), self.settings.factory.clone()) else {
            return;
        };
        if instance.build.as_ref().is_some_and(|(_, c)| *c == config) {
            return;
        }
        instance.build = Some((id, config.clone()));
        let span = instance.rt.as_ref().map_or_else(|| self.span.clone(), |rt| rt.device.span.clone());
        let device = DeviceConfig::new(name, driver_fields(&config));
        self.spawner.build(factory, device, id, self.settings.factory_timeout, span);
    }

    /// A construction is done.
    async fn built(
        &mut self,
        ctx: Option<&Ctx<'_>>,
        outputs: &mut Outputs,
        name: &str,
        id: u64,
        result: Result<(Box<dyn Startable>, DriverInfo), FactoryError>,
    ) -> Result<(), Failed> {
        let stale = self.instances.get(name).map_or(true, |i| i.build.as_ref().map(|(b, _)| *b) != Some(id));
        if stale {
            if let Ok((driver, _)) = result {
                drop_off_thread(driver);
            }
            return Ok(());
        }
        let Some((_, config)) = self.instances.get_mut(name).and_then(|i| i.build.take()) else {
            return Ok(());
        };
        // The driver's timing, with the host's overrides; timings that cannot run are the Config's
        // failure.
        let result = result.and_then(|(driver, info)| {
            match config::device_info(info, &self.settings.overrides, self.settings.device_stable, &self.settings.mqtt) {
                Ok(info) => Ok((driver, info)),
                Err(e) => {
                    drop_off_thread(driver);
                    Err(FactoryError::Unusable(format!("the driver's timing cannot run: {e}")))
                }
            }
        });
        match result {
            Err(e) => {
                let (reason, error) = match e {
                    FactoryError::Malformed(m) => (Reason::Malformed, m),
                    FactoryError::Unusable(m) => (Reason::Unusable, m),
                };
                // One `Error` per Config: a retry of one that failed already says nothing new.
                let retried = self.instances.get(name).is_some_and(|i| i.failed_config.as_ref() == Some(&config));
                if retried {
                    debug!(instance = name, reason = reason.as_str(), error = %error, "A failed instance's construction failed again; retrying at the reconnect pacing");
                } else {
                    self.config_rejected(outputs, name, reason, &error);
                }
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.failed_config = Some(config);
                }
                self.mark_failed(name);
            }
            Ok((driver, info)) => {
                // A disable's validation: the fields are valid; the driver is not needed.
                if self.instances.get(name).is_some_and(|i| i.want == Want::Disable(config.clone())) {
                    drop_off_thread(driver);
                    if let Some(instance) = self.instances.get_mut(name) {
                        instance.validated = Some(config.clone());
                    }
                    // Validated, the disable takes over: a stop under way ends disabled, and a
                    // committed driver waiting to start is dropped (`set_want`).
                    self.set_want(name, Want::Disable(config));
                    return self.reconcile(ctx, outputs, name).await;
                }
                let wanted = self.instances.get(name).is_some_and(|i| i.want == Want::Serve(config.clone()));
                if !wanted {
                    drop_off_thread(driver);
                    return Ok(());
                }
                if let Some(instance) = self.instances.get_mut(name) {
                    // It replaces a driver kept ready for the Config committed before it.
                    if let Some(kept) = instance.ready.replace(Ready { config, driver, info }) {
                        drop_off_thread(kept.driver);
                    }
                }
                self.reconcile(ctx, outputs, name).await?;
            }
        }
        Ok(())
    }

    /// With nothing serving the instance, a construction's failure makes it failed:
    /// `Status=unreachable`, retried at the reconnect pacing (multi-device spec §4.3).
    fn mark_failed(&mut self, name: &str) {
        // At the reconnect pacing, and at most once a second: a thread is spent on each.
        let retry = self.settings.retry.max(CONSTRUCTION_RETRY_FLOOR);
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        // Something serves it, or is ready to: the last accepted Config stays in force.
        if !instance.failed() {
            return;
        }
        let Some(rt) = instance.rt.as_ref() else {
            return;
        };
        rt.device.reports.status(LinkStatus::Unreachable);
        if instance.failed_config.is_some() {
            instance.retry_at = Some(Instant::now() + retry);
        }
        if self.next_sweep.is_none() {
            self.next_sweep = Some(Instant::now() + SWEEP_EVERY);
        }
    }

    /// A driver built for the wanted Config: committed (multi-device spec §4.3) — attached to a
    /// device that has none, the current device stopped for its successor, or a disabled
    /// instance enabled again from a fresh bootstrap of S2.
    async fn commit(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let Some(instance) = self.instances.get_mut(name) else {
            return Ok(());
        };
        let Some(config) = instance.ready.as_ref().map(|r| r.config.clone()) else {
            return Ok(());
        };
        let Some((phase, worker)) = instance.rt.as_ref().map(|rt| (rt.phase, rt.worker)) else {
            return Ok(());
        };
        instance.accepted = Some(config);
        instance.failed_config = None;
        instance.retry_at = None;
        match (phase, worker) {
            (Phase::Live, false) => {
                if let Some(rt) = instance.rt.as_ref() {
                    rt.device.reports.status(LinkStatus::Connecting);
                }
                self.start_if_ready(ctx, outputs, name).await?;
            }
            (Phase::Live, true) => self.begin_stop(ctx, outputs, name, Then::Successor).await?,
            // (A reconcile has left Disabled before building; kept for any other way here.)
            (Phase::Disabled, _) => {
                self.re_enable(ctx, outputs, name).await?;
                self.start_if_ready(ctx, outputs, name).await?;
            }
            (Phase::Stopping { .. }, _) => {}
        }
        Ok(())
    }

    /// A disabled instance enabled again: a new device, its read-back seeded from what S1 — still
    /// subscribed — has shown of the slot, frozen at S2's SUBACK; then S2 and S3.
    async fn re_enable(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let Some(old) = self.instances.get_mut(name).and_then(|i| i.rt.take()) else {
            return Ok(());
        };
        retire_slot(&self.shared, outputs, old.id, name, self.conn.connected && ctx.is_some());
        self.create_rt(None, outputs, name, Phase::Live, LinkStatus::Connecting).await?;
        let connected = self.conn.connected;
        let own = self.settings.id.clone();
        let Some(instance) = self.instances.get_mut(name) else {
            return Ok(());
        };
        let Some(rt) = instance.rt.as_mut() else {
            return Ok(());
        };
        rt.read = old.read;
        // Only what carries this process's own tag — the effective owner's (§4.2).
        for (topic, seen) in &instance.slot {
            if let (Inbound::State { target }, true) = (instance.topics.parse(topic), seen.via.as_deref() == Some(own.as_str())) {
                if let Ok(values) = wire::decode_state(&seen.payload) {
                    rt.device.reports.read_back(&target, values);
                }
            }
        }
        let (id, read) = (rt.id, rt.read);
        if connected && read == Sub::Granted {
            // Its output may go out: its new `Status` (all its model holds) through the rotation,
            // within the packet-id budget while a request is outstanding (`Conn`).
            if ctx.is_some() {
                outputs.announce(id);
            }
            self.request_setup(ctx, name).await?;
        } else if connected {
            // S1's grant was still awaited for the disabled runtime: this one asks for its own.
            self.request_read(ctx, name).await?;
        }
        Ok(())
    }

    /// A new device runtime for `name` — a fresh mailbox and reports, `status` — in the rotation,
    /// its bootstrap begun if connected.
    async fn create_rt(
        &mut self,
        ctx: Option<&Ctx<'_>>,
        outputs: &mut Outputs,
        name: &str,
        phase: Phase,
        status: LinkStatus,
    ) -> Result<(), Failed> {
        let id = self.next();
        let Some(instance) = self.instances.get_mut(name) else {
            return Ok(());
        };
        let span = tracing::info_span!(parent: &self.span, "device", instance = %name);
        let reports = Reports::with_ready(self.ready.clone());
        reports.status(status);
        let device = Arc::new(Device {
            topics: instance.topics.clone(),
            commands: std::sync::OnceLock::new(),
            mailbox: Mailbox::new(),
            reports,
            tag: Some(self.settings.id.clone()),
            span,
        });
        instance.rt = Some(Rt {
            id,
            device: device.clone(),
            phase,
            worker: false,
            read: Sub::None,
            setup: Sub::None,
            requests: Sub::None,
            bootstrapped: false,
            unsubscribing: 0,
        });
        instance.installed.clear();
        outputs.add(&self.shared, id, device);
        self.request_read(ctx, name).await
    }

    /// Start the worker, once a driver is ready and S3 has been granted (multi-device spec §4.3:
    /// target Configs first, then the device is eligible to execute, then S3's replay).
    async fn start_if_ready(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let Some(instance) = self.instances.get_mut(name) else {
            return Ok(());
        };
        let eligible = instance.rt.as_ref().is_some_and(|rt| rt.phase == Phase::Live && !rt.worker && rt.bootstrapped);
        if !eligible || instance.ready.is_none() {
            return Ok(());
        }
        let Some(ready) = instance.ready.take() else {
            return Ok(());
        };
        let Some(rt) = instance.rt.as_mut() else {
            return Ok(());
        };
        let _ = rt.device.commands.set(ready.info.commands.clone());
        let device = rt.device.clone();
        match self.spawner.start(
            rt.id,
            name,
            true,
            ready.driver,
            ready.info,
            device.mailbox.clone(),
            device.reports.clone(),
            device.span.clone(),
        ) {
            Ok(()) => {
                rt.worker = true;
                // A newer Config that failed meanwhile — a disable's invalid fields, while this
                // committed one waited to start — stays failed: not built, nor said, again.
                if instance.failed_config.as_ref() == Some(&ready.config) {
                    instance.failed_config = None;
                }
                instance.accepted = Some(ready.config);
                instance.retry_at = None;
                info!(instance = name, "The device is started");
            }
            Err(e) => {
                let config = ready.config;
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.failed_config = Some(config);
                    instance.accepted = None;
                }
                self.config_rejected(outputs, name, Reason::Unusable, &e);
                self.mark_failed(name);
            }
        }
        let _ = ctx;
        Ok(())
    }

    // ---- Stopping a device -----------------------------------------------------------------

    /// Stop the instance's device (multi-device spec §4.4): admission closes now — a delivery for
    /// it from here on gets the terminal policy — and its worker is asked to stop.
    async fn begin_stop(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str, then: Then) -> Result<(), Failed> {
        let connected = self.conn.connected;
        let Some(rt) = self.instances.get_mut(name).and_then(|i| i.rt.as_mut()) else {
            return Ok(());
        };
        info!(instance = name, then = ?then, "Stopping the device");
        rt.phase = Phase::Stopping { step: Step::Worker, then };
        // Its output may go out now: the read-back no longer matters.
        if connected {
            outputs.announce(rt.id);
        }
        if rt.worker && self.spawner.registry.stop(rt.id) {
            return Ok(());
        }
        self.worker_stopped(ctx, outputs, name).await
    }

    /// The worker has stopped (R1's stop): what waited for it is answered — momentary commands and
    /// reads refused, the one it ran "cut off"; held Desired `superseded` on a Config change,
    /// `refused` otherwise — and its output retires through the rotation.
    async fn worker_stopped(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let Some(rt) = self.instances.get_mut(name).and_then(|i| i.rt.as_mut()) else {
            return Ok(());
        };
        let Phase::Stopping { then, .. } = rt.phase else {
            return Ok(());
        };
        rt.worker = false;
        let device = &rt.device;
        crate::worker::report_refused(&device.reports, device.mailbox.disconnected(), STOPPED_BEFORE);
        let (reason, error) = match then {
            Then::Successor => (Reason::Superseded, "the instance's Config changed: a new device serves it, from its own replay"),
            Then::Disable => (Reason::Refused, "the instance was disabled before the request was applied"),
            Then::Release { .. } => (Reason::Refused, "the instance was deleted or moved before the request was applied"),
        };
        for value in device.mailbox.drain_values() {
            device.reports.request_error(ErrorReport::new(reason, error).target(Some(&value.target)).property(&value.property));
        }
        rt.phase = Phase::Stopping { step: Step::Retire, then };
        // Its retirement output is frozen now — finite: what its reports hold goes into its slot's
        // batch, and the retirement waits for that batch alone. Later requests' terminal answers go
        // to the outbox (`terminal`), so no flood can keep the stop open (R2 gate, Codex 2).
        let (id, device) = (rt.id, rt.device.clone());
        if let Some(slot) = outputs.slot(id) {
            slot.batch.add(&device, device.reports.take());
        }
        self.check_retired(ctx, outputs, name).await
    }

    /// A stopping device whose output has retired — published, or lost with the session — has its
    /// subscriptions removed: S1–S3, or S2 and S3 for a disable.
    async fn check_retired(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let connected = self.conn.connected && ctx.is_some();
        let Some(rt) = self.instances.get_mut(name).and_then(|i| i.rt.as_mut()) else {
            return Ok(());
        };
        let Phase::Stopping { step: Step::Retire, then } = rt.phase else {
            return Ok(());
        };
        let id = rt.id;
        let device = rt.device.clone();
        if connected {
            // A stopping device's output may go out on any connection — this one may be newer than
            // the stop, and began with nothing announced.
            outputs.announce(id);
            // The frozen output alone (`worker_stopped`).
            let pending = outputs.slot(id).is_some_and(|slot| !slot.batch.is_empty());
            if pending {
                return Ok(());
            }
        } else {
            let mut lost = outputs.slot(id).map_or(0, |slot| std::mem::take(&mut slot.batch).len());
            if device.reports.has_untaken() {
                let mut batch = Batch::default();
                batch.add(&device, device.reports.take());
                lost += batch.len();
            }
            if lost > 0 {
                warn!(
                    kind = "output_lost_at_session_end",
                    instance = name,
                    lost,
                    "A stopping device's output was not published before the session ended; dropped"
                );
            }
        }
        rt.phase = Phase::Stopping { step: Step::Unsubscribe, then };
        let (read, setup, requests) = (rt.read, rt.setup, rt.requests);
        let topics = device.topics.clone();
        let mut filters: Vec<String> = Vec::new();
        if setup != Sub::None {
            filters.push(topics.config_target_filter());
        }
        if requests != Sub::None {
            filters.extend([topics.desired_property_filter(), topics.desired_target_filter(), topics.command()]);
        }
        if then != Then::Disable && read != Sub::None {
            filters.extend([topics.state_target_filter(), topics.status()]);
        }
        let mut awaiting = 0;
        if connected {
            for filter in filters {
                if self.request_unsubscription(ctx, Purpose::Unsub { instance: name.to_owned(), rt: id }, filter).await? {
                    awaiting += 1;
                }
            }
        }
        if let Some(rt) = self.instances.get_mut(name).and_then(|i| i.rt.as_mut()) {
            rt.unsubscribing = awaiting;
        }
        if awaiting == 0 {
            self.finish_stop(ctx, outputs, name).await?;
        }
        Ok(())
    }

    /// The stop is done, its UNSUBACKs in: the instance becomes what the stop was for — and only
    /// now does a successor bootstrap, so no old replay reaches it.
    async fn finish_stop(&mut self, ctx: Option<&Ctx<'_>>, outputs: &mut Outputs, name: &str) -> Result<(), Failed> {
        let connected = self.conn.connected && ctx.is_some();
        let id = self.settings.id.clone();
        let Some(instance) = self.instances.get_mut(name) else {
            return Ok(());
        };
        let Some(Phase::Stopping { then, .. }) = instance.rt.as_ref().map(|rt| rt.phase) else {
            return Ok(());
        };
        match then {
            Then::Disable => {
                // It keeps its read-back — on a connection newer than the stop, the CONNACK asked
                // for it again (`on_connack`).
                if let Some(rt) = instance.rt.as_mut() {
                    rt.phase = Phase::Disabled;
                    rt.setup = Sub::None;
                    rt.requests = Sub::None;
                    rt.device.reports.status(LinkStatus::Disabled);
                }
                info!(instance = name, "The instance is disabled");
            }
            Then::Successor | Then::Release { .. } => {
                if let Some(old) = instance.rt.take() {
                    retire_slot(&self.shared, outputs, old.id, name, connected);
                }
                if let Then::Release { cleanup: true } = then {
                    // Its own tagged topics, as its read-back last saw them, retracted — now, or
                    // after the next root replay if the broker is away.
                    let own: Vec<String> =
                        instance.slot.iter().filter(|(_, seen)| seen.via.as_deref() == Some(id.as_str())).map(|(t, _)| t.clone()).collect();
                    if own.is_empty() {
                    } else if connected {
                        for topic in &own {
                            outputs.queue_outbox(Part::Model, Out::new(topic.clone(), true, Vec::new(), None, Some(&id)));
                        }
                        self.queued_cleanups.push((name.to_owned(), None));
                    } else {
                        info!(
                            instance = name,
                            topics = own.len(),
                            "A deleted instance's retained topics are cleared after the next root replay"
                        );
                        self.cleanups.insert(name.to_owned());
                    }
                }
                let Some(instance) = self.instances.get_mut(name) else {
                    return Ok(());
                };
                instance.slot.clear();
                if then == Then::Successor {
                    // Its bootstrap begins now; the driver built for it starts at S3's grant — or,
                    // if the wanted Config changed meanwhile, the reconcile below builds anew.
                    info!(instance = name, "The device is stopped; its successor bootstraps");
                    self.create_rt(ctx, outputs, name, Phase::Live, LinkStatus::Connecting).await?;
                } else {
                    info!(instance = name, "The instance is released");
                    if matches!(self.instances.get(name).map(|i| &i.want), Some(Want::Leave { .. })) {
                        self.instances.remove(name);
                        return Ok(());
                    }
                }
            }
        }
        // Boxed: a reconcile may stop a device again, whose stop ends here.
        Box::pin(self.reconcile(ctx, outputs, name)).await
    }

    // ---- Routing ---------------------------------------------------------------------------

    /// A publish on one of an owned instance's lanes.
    fn route_instance(&mut self, outputs: &mut Outputs, name: &str, topic: &str, publish: &Publish, received: Instant) {
        let id = self.settings.id.clone();
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        let inbound = instance.topics.parse(topic);
        if matches!(inbound, Inbound::State { .. } | Inbound::Status) {
            observe_slot(instance, &id, topic, &inbound, publish);
            return;
        }
        let failed = instance.failed();
        let Some(rt) = instance.rt.as_ref() else {
            debug!(topic, "A request for an instance with no device; ignored");
            return;
        };
        let device = rt.device.clone();
        match rt.phase {
            Phase::Stopping { .. } => terminal(
                &device,
                outputs,
                topic,
                publish,
                "the instance's device is being stopped (its Config changed, or it was disabled, deleted or moved)",
            ),
            Phase::Disabled => terminal(&device, outputs, topic, publish, "the instance is disabled"),
            Phase::Live => {
                if let Inbound::Config { target } = &inbound {
                    // Present on this connection: replayed, or written live since S2 was asked for
                    // — a target Config the Store creates in S2's window is retained though no
                    // replay brought it (R2 gate, Codex 5 / Claude 2); a live retraction is gone.
                    if publish.payload.is_empty() {
                        instance.installed.remove(target);
                        instance.replayed.remove(target);
                    } else {
                        instance.installed.insert(target.clone());
                        instance.replayed.insert(target.clone());
                    }
                }
                // A failed instance refuses target-level writes, as commands (multi-device spec §4.3).
                if failed && matches!(inbound, Inbound::DesiredTarget { .. }) {
                    return terminal(&device, outputs, topic, publish, "the instance's device could not be built from its Config");
                }
                let _entered = device.span.enter();
                session::route(&device, topic, publish, received);
            }
        }
    }
}

/// A delivery on an owned instance's read-back (S1): the slot's view updated; a retained `State`
/// with exactly one `via`, read back until the read-back freezes; and — while the instance is owned
/// and not stopping — a write tagged with another `via` (or none, or several), or an empty
/// retraction, makes the owner republish its current value for that topic, coalesced (multi-device
/// spec §4.5). A topic it has no current value for is left as it is.
fn observe_slot(instance: &mut Instance, id: &str, topic: &str, inbound: &Inbound, publish: &Publish) {
    let via = single_via(publish);
    let payload = &publish.payload[..];
    if payload.is_empty() {
        instance.slot.remove(topic);
    } else {
        instance.slot.insert(topic.to_owned(), Seen { via: via.clone(), payload: payload.to_vec() });
    }
    let Some(rt) = instance.rt.as_ref() else {
        return;
    };
    if matches!(rt.phase, Phase::Stopping { .. }) {
        return;
    }
    let reports = &rt.device.reports;
    // Only the effective owner's own tag is a "before" value (§4.2): a State another process
    // tagged is not this owner's knowledge of the device (R2 gate, Codex 6).
    if let (Inbound::State { target }, true, true) = (inbound, publish.retain, via.as_deref() == Some(id)) {
        match wire::decode_state(payload) {
            Ok(values) => reports.read_back(target, values),
            Err(e) => {
                debug!(instance = %instance.name, target = %target, error = %e, "The retained state read back is not a state; ignored")
            }
        }
    }
    if !payload.is_empty() && via.as_deref() == Some(id) {
        return;
    }
    let reasserted = match inbound {
        Inbound::Status => reports.reassert_status(),
        Inbound::State { target } => reports.reassert_state(target),
        _ => false,
    };
    if reasserted && !payload.is_empty() {
        info!(kind = "slot_reasserted", instance = %instance.name, topic, via = via.as_deref().unwrap_or("none"), "A foreign write on an owned slot; republishing the owner's value");
    } else if reasserted {
        debug!(instance = %instance.name, topic, "An owned slot was retracted; republishing the owner's value");
    }
}

/// A publish's `via` user property: exactly one, or none (none or several — §3).
fn single_via(publish: &Publish) -> Option<String> {
    let vias: Vec<&str> = publish
        .properties
        .as_ref()
        .map(|p| p.user_properties.iter().filter(|(k, _)| k == "via").map(|(_, v)| v.as_str()).collect())
        .unwrap_or_default();
    (vias.len() == 1).then(|| vias[0].to_owned())
}

/// The terminal policy of an instance that takes no requests — stopping, or disabled (multi-device
/// spec §4.4 step 1): commands and reads refused, with their replies; target-level Desired refused;
/// property Desired ignored (retained: it replays to a successor); target Configs ignored (they
/// replay too). Nothing is posted to any mailbox.
///
/// The answers go to the session's outbox, a member of the fair rotation — not to the device's
/// reports: a stopping device's retirement output is frozen at its worker's stop, so requests that
/// keep arriving through its still-installed subscriptions can never hold the stop open (R2 gate,
/// Codex 2).
fn terminal(device: &Device, outputs: &mut Outputs, topic: &str, publish: &Publish, why: &str) {
    let _entered = device.span.enter();
    let payload = &publish.payload[..];
    let error = |outputs: &mut Outputs, report: ErrorReport| {
        outputs.queue_outbox(Part::Answers, Out::new(device.topics.error(), false, report.encode(), None, None));
    };
    match device.topics.parse(topic) {
        Inbound::Command => {
            let (target, command) = wire::command_names(payload);
            error(outputs, ErrorReport::new(Reason::Refused, why).target(target.as_deref()).command(&command));
            if let Ok(Some(reply)) = ReplyTo::from_properties(publish.properties.as_ref(), &device.topics) {
                let answer = Answer::Failed { reason: Reason::Refused, error: why.to_owned() };
                let payload = reply_payload(target.as_deref(), &command, &answer);
                outputs.queue_outbox(Part::Answers, Out::new(reply.topic, false, payload, reply.correlation, None));
            }
            debug!(topic, command = %command, "A command for an instance that takes none; refused");
        }
        Inbound::DesiredTarget { target } => {
            error(outputs, ErrorReport::new(Reason::Refused, why).target(Some(&target)));
            debug!(topic, "A target-level write for an instance that takes none; refused");
        }
        Inbound::DesiredProperty { .. } | Inbound::Config { .. } => {
            debug!(topic, "A retained request for an instance that takes none; ignored (it replays to a successor)");
        }
        Inbound::State { .. } | Inbound::Status | Inbound::Unknown => {}
    }
}

/// A device runtime leaves the rotation (multi-device spec §4.4 step 3): the answers and unsolicited
/// reports it still holds — refusals of deliveries that reached it as it unsubscribed — go to the
/// outbox rather than with its runtime; its model is no longer its to write. With no connection they
/// are lost, as a session's output is (WARN `output_lost_at_session_end`).
fn retire_slot(shared: &session::Shared, outputs: &mut Outputs, id: u64, name: &str, connected: bool) {
    let Some(mut slot) = outputs.remove(shared, id) else {
        return;
    };
    slot.batch.add(&slot.device, slot.device.reports.take());
    let answers = slot.batch.drain(Part::Answers);
    let unsolicited = slot.batch.drain(Part::Unsolicited);
    if connected {
        for out in answers {
            outputs.queue_outbox(Part::Answers, out);
        }
        for out in unsolicited {
            outputs.queue_outbox(Part::Unsolicited, out);
        }
    } else if !answers.is_empty() || !unsolicited.is_empty() {
        warn!(
            kind = "output_lost_at_session_end",
            instance = name,
            lost = answers.len() + unsolicited.len(),
            "A stopped device's answers were not published before the session ended; dropped"
        );
    }
}

/// What a device stop refuses: a command still waiting for it.
const STOPPED_BEFORE: &str = "the device was stopped before it ran (its Config changed, or the instance was disabled, deleted or moved)";

/// (tests) A fixed host's lifecycle serving `device` (its instance the bridge id), no worker
/// behind it: the session's own tests drive a fixed bridge with it.
#[cfg(test)]
pub(crate) fn fixed_for_test(shared: Arc<session::Shared>, outputs: &mut Outputs, device: Arc<Device>) -> Lifecycle {
    let ready = shared.ready.clone();
    let settings = Settings {
        kind: "test".into(),
        id: device.topics.instance().to_owned(),
        root: device.topics.root().to_owned(),
        factory: None,
        factory_timeout: config::FACTORY_TIMEOUT,
        retry: Duration::from_secs(1),
        overrides: TimingOverrides::default(),
        device_stable: None,
        mqtt: shared.timing,
    };
    let (spawner, _, _) = Spawner::new();
    let mut host = Lifecycle::new(settings, shared, spawner, Span::none(), ready);
    host.insert_fixed(outputs, device);
    host
}

#[cfg(test)]
mod tests;

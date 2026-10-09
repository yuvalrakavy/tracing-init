//! The multi-device host's per-instance lifecycle (multi-device spec §3, §4.2–§4.6): the router,
//! the ownership by `Config/{instance}`, the per-instance subscriptions and their bootstrap, the
//! stop path, the retained topics' tags, healing and cleanup, and the startup's `Active` gate —
//! the session part of a host, driven by the session's events ([`crate::session`]).
//!
//! **Ownership** (§3, §4.2). `Config/+` and then a barrier, `{Root}/_barrier/{bridge id}`, are
//! subscribed on every CONNACK; the barrier's SUBACK closes the root replay. An instance is owned
//! iff its last accepted Config names this process (`via`, or omitted, the kind) and its kind.
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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
use crate::reports::{LinkStatus, Outbound, Reports};
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
    /// It names another process (`via`, or the kind's default bridge id).
    Foreign(String),
    /// It names this process, and another kind.
    KindMismatch { config: Value, kind: String },
    /// It names this process, and the host's own fields are invalid.
    Malformed { config: Value, error: String },
    /// This process's: the parsed Config (compared whole), whether it is disabled, and the driver's
    /// fields.
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
    // The effective owner: `via`, else the kind's default bridge id — the kind itself.
    let owner = match fields.get("via") {
        None => kind.to_owned(),
        Some(Value::String(via)) => via.clone(),
        Some(_) => return Verdict::Unreadable("its `via` is not a string".into()),
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
    Verdict::Ours { config, disabled, fields }
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
    /// The read-back's view of the instance's slot: each topic's current retained value as last
    /// seen.
    slot: BTreeMap<String, Seen>,
    /// The target Configs given to the current device (non-empty last).
    installed: BTreeSet<String>,
    /// The target Configs the current connection's S2 replayed.
    replayed: BTreeSet<String>,
}

impl Instance {
    fn failed(&self) -> bool {
        self.rt.as_ref().is_some_and(|rt| rt.phase == Phase::Live && !rt.worker) && self.accepted.is_none() && self.build.is_none()
    }
}

/// What a request awaiting its acknowledgement is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Purpose {
    Configs,
    Barrier,
    Read { instance: String, rt: u64 },
    Setup { instance: String, rt: u64 },
    Requests { instance: String, rt: u64 },
    Unsub { instance: String, rt: u64 },
}

struct Pending {
    purpose: Purpose,
    unsubscribe: bool,
    deadline: Instant,
}

/// One connection's subscriptions.
#[derive(Default)]
struct Conn {
    connected: bool,
    /// Requests handed to rumqttc whose packet id is not seen yet, in order.
    queued: VecDeque<Pending>,
    /// Requests awaiting their acknowledgement, by packet id — a queue each: rumqttc shares one
    /// packet-id space with the publishes and wraps it, so an id may come round again while an
    /// earlier request with it waits, and a broker answers in order.
    awaiting: HashMap<u16, VecDeque<Pending>>,
    configs_granted: Option<Instant>,
    barrier_acked: bool,
    /// Instances whose `Config/{i}` came on this connection.
    replayed: BTreeSet<String>,
    active: bool,
}

impl Conn {
    /// The subscriptions requested and not yet granted, the barrier's aside: `Active=true` waits
    /// for every other grant, and for the barrier only up to the replay's fallback (multi-device
    /// spec §4.6) — counted here, the fallback could never apply.
    fn pending_subscriptions(&self) -> usize {
        self.queued.iter().chain(self.awaiting.values().flatten()).filter(|p| !p.unsubscribe && p.purpose != Purpose::Barrier).count()
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.queued.iter().chain(self.awaiting.values().flatten()).map(|p| p.deadline).min()
    }
}

/// What a host is, for its lifecycle.
pub(crate) struct Settings {
    pub kind: String,
    pub id: String,
    pub root: String,
    pub factory: Factory,
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
        [self.conn.next_deadline(), fallback, self.next_sweep].into_iter().flatten().chain(retries).min()
    }

    // ---- Subscriptions ---------------------------------------------------------------------

    /// Subscribe `filters` for `purpose`: queued until rumqttc gives its packet id, then awaited.
    async fn request_subscription(&mut self, ctx: Option<&Ctx<'_>>, purpose: Purpose, filters: Vec<Filter>) -> Result<(), Failed> {
        let Some(ctx) = ctx.filter(|_| self.conn.connected) else {
            return Ok(());
        };
        self.conn.queued.push_back(Pending { purpose, unsubscribe: false, deadline: Instant::now() + GRANT_BOUND });
        // WAIT: mqtt-request
        ctx.client.subscribe_many(filters).await
    }

    /// Unsubscribe `filter` for `purpose`.
    async fn request_unsubscription(&mut self, ctx: Option<&Ctx<'_>>, purpose: Purpose, filter: String) -> Result<bool, Failed> {
        let Some(ctx) = ctx.filter(|_| self.conn.connected) else {
            return Ok(false);
        };
        self.conn.queued.push_back(Pending { purpose, unsubscribe: true, deadline: Instant::now() + GRANT_BOUND });
        // WAIT: mqtt-request
        ctx.client.unsubscribe(filter).await.map(|()| true)
    }

    fn filter(path: String, retain_as_published: bool) -> Filter {
        Filter { preserve_retain: retain_as_published, ..Filter::new(path, QoS::AtLeastOnce) }
    }

    /// S1, the read-back: the instance's targets' `State` and its `Status`, kept while it is owned.
    async fn request_read(&mut self, ctx: Option<&Ctx<'_>>, name: &str) -> Result<(), Failed> {
        let Some((id, topics)) = self.rt_of(name).map(|(rt, topics)| (rt.id, topics.clone())) else {
            return Ok(());
        };
        if !self.conn.connected {
            return Ok(());
        }
        self.set_sub(name, |rt| rt.read = Sub::Requested);
        let filters = vec![Self::filter(topics.state_target_filter(), false), Self::filter(topics.status(), false)];
        self.request_subscription(ctx, Purpose::Read { instance: name.to_owned(), rt: id }, filters).await
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
        let root = &self.settings.root;
        let configs = Self::filter(format!("{root}/Config/+"), false);
        let barrier = Self::filter(format!("{root}/_barrier/{}", self.settings.id), false);
        self.request_subscription(Some(ctx), Purpose::Configs, vec![configs]).await?;
        self.request_subscription(Some(ctx), Purpose::Barrier, vec![barrier]).await?;
        let names: Vec<String> = self.instances.keys().cloned().collect();
        for name in names {
            let phase = self.instances.get(&name).and_then(|i| i.rt.as_ref()).map(|rt| rt.phase);
            if matches!(phase, Some(Phase::Live | Phase::Disabled)) {
                self.request_read(Some(ctx), &name).await?;
            }
        }
        Ok(())
    }

    /// A SUBSCRIBE or UNSUBSCRIBE written (its packet id), or its acknowledgement.
    pub(crate) async fn on_ack(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, event: PumpEvent) -> Result<Flow, Failed> {
        match event {
            PumpEvent::Subscribing(pkid) | PumpEvent::Unsubscribing(pkid) => {
                match self.conn.queued.pop_front() {
                    Some(pending) => self.conn.awaiting.entry(pkid).or_default().push_back(pending),
                    None => debug!(pkid, "A subscription request the lifecycle did not make"),
                }
                Ok(Flow::Continue)
            }
            PumpEvent::SubAck { pkid, granted } | PumpEvent::UnsubAck { pkid, granted } => {
                let Some(pending) = self.conn.awaiting.get_mut(&pkid).and_then(VecDeque::pop_front) else {
                    debug!(pkid, "An acknowledgement of nothing awaited");
                    return Ok(Flow::Continue);
                };
                if self.conn.awaiting.get(&pkid).is_some_and(VecDeque::is_empty) {
                    self.conn.awaiting.remove(&pkid);
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
                self.maybe_active(ctx).await;
                Ok(Flow::Continue)
            }
            _ => Ok(Flow::Continue),
        }
    }

    async fn granted(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, purpose: Purpose) -> Result<(), Failed> {
        match purpose {
            Purpose::Configs => self.conn.configs_granted = Some(Instant::now()),
            Purpose::Barrier => {
                self.conn.barrier_acked = true;
                self.replay_complete(Some(ctx), outputs).await?;
            }
            Purpose::Read { instance, rt } => {
                let Some(device) = self.current(&instance, rt).map(|rt| rt.device.clone()) else {
                    return Ok(());
                };
                self.set_sub(&instance, |rt| rt.read = Sub::Granted);
                // Its slot is read: its output may go out, its current Status and State first —
                // before `Active=true` (multi-device spec §4.6).
                outputs.announce(rt);
                session::republish(ctx.shared, ctx.client, &device, device.reports.model()).await;
                if self.current(&instance, rt).is_some_and(|rt| rt.phase == Phase::Live) {
                    self.request_setup(Some(ctx), &instance).await?;
                }
            }
            Purpose::Setup { instance, rt } => {
                let Some(device) = self.current(&instance, rt).map(|rt| rt.device.clone()) else {
                    return Ok(());
                };
                self.set_sub(&instance, |rt| rt.setup = Sub::Granted);
                // S2's SUBACK closes S1's replay: the conflict snapshot freezes.
                device.reports.read_back_complete();
                if self.current(&instance, rt).is_some_and(|rt| rt.phase == Phase::Live) {
                    self.request_requests(Some(ctx), &instance).await?;
                }
            }
            Purpose::Requests { instance, rt } => {
                if self.current(&instance, rt).is_none() {
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
            Purpose::Unsub { .. } => {}
        }
        Ok(())
    }

    async fn unsubscribed(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, purpose: Purpose) -> Result<(), Failed> {
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
    /// `Config/+` grant (multi-device spec §4.6). A pending grant is never bypassed.
    async fn maybe_active(&mut self, ctx: &Ctx<'_>) {
        if self.conn.active || !self.conn.connected || self.conn.pending_subscriptions() > 0 {
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
        self.conn.active = true;
        let shared = ctx.shared;
        session::emit(shared, ctx.client, shared.topics.active(), true, b"true".to_vec(), session::properties()).await;
        session::emit(shared, ctx.client, shared.topics.version(), true, shared.version.clone().into_bytes(), session::properties()).await;
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
        let Some(rest) = topic.strip_prefix(self.settings.root.as_str()).and_then(|r| r.strip_prefix('/')) else {
            debug!(topic, "A publish off the root; ignored");
            return Ok(Flow::Continue);
        };
        let segments: Vec<&str> = rest.split('/').collect();
        match segments.as_slice() {
            ["Config", instance] => self.config(Some(ctx), outputs, instance, publish).await?,
            ["_barrier", ..] => {}
            [_, instance, ..] if self.instances.contains_key(*instance) => self.route_instance(instance, topic, publish, received),
            _ => debug!(topic, "A publish for no instance this process owns; ignored"),
        }
        Ok(Flow::Continue)
    }

    /// A device's keeper or a construction's watcher, during a session.
    pub(crate) async fn on_event(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs, event: Event) -> Result<Flow, Failed> {
        self.handle_event(Some(ctx), outputs, event).await.map(|()| Flow::Continue)
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

    /// A timer: an overdue grant ends the session; the replay's fallback; a failed instance's
    /// retry; the sweep of failed instances' held requests.
    pub(crate) async fn on_timer(&mut self, ctx: &Ctx<'_>, outputs: &mut Outputs) -> Result<Flow, Failed> {
        let now = Instant::now();
        if let Some(overdue) = self.conn.queued.iter().chain(self.conn.awaiting.values().flatten()).find(|p| p.deadline <= now) {
            warn!(
                kind = "subscription_refused",
                purpose = ?overdue.purpose,
                unsubscribe = overdue.unsubscribe,
                overdue_ms = GRANT_BOUND.as_millis() as u64,
                "The broker did not acknowledge a subscription the host needs in time; ending the session"
            );
            return Ok(Flow::End);
        }
        self.maybe_active(ctx).await;
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
        let retiring: Vec<String> = self
            .instances
            .values()
            .filter(|i| i.rt.as_ref().is_some_and(|rt| matches!(rt.phase, Phase::Stopping { step: Step::Retire, .. })))
            .map(|i| i.name.clone())
            .collect();
        for name in retiring {
            self.check_retired(Some(ctx), outputs, &name).await?;
        }
        Ok(Flow::Continue)
    }

    /// The connection is lost: its subscriptions are gone with it. A stopping device's output not
    /// yet published is dropped (WARN `output_lost_at_session_end`), as R1 drops unpublished output
    /// at process shutdown; the next CONNACK bootstraps every owned instance again.
    pub(crate) async fn on_session_lost(&mut self, outputs: &mut Outputs) {
        self.conn = Conn::default();
        outputs.outbox_open = false;
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
                // Not served: released, if this process served it.
                if owned {
                    self.set_want(name, Want::Leave { cleanup: false });
                    self.reconcile(ctx, outputs, name).await?;
                }
            }
            Verdict::Malformed { config, error } => {
                if self.instances.get(name).is_some_and(|i| i.last_seen.as_ref() == Some(&config)) {
                    return Ok(());
                }
                self.own(name);
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.last_seen = Some(config);
                    // The last accepted Config stays in force; none — the instance is failed.
                    if !owned {
                        instance.want = Want::Broken;
                    }
                }
                self.config_rejected(outputs, name, Reason::Malformed, &error);
                self.reconcile(ctx, outputs, name).await?;
                self.mark_failed(name);
            }
            Verdict::Ours { config, disabled, .. } => {
                if self.instances.get(name).is_some_and(|i| i.last_seen.as_ref() == Some(&config)) {
                    return Ok(());
                }
                self.rejected.remove(name);
                self.own(name);
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
    fn own(&mut self, name: &str) {
        if self.instances.contains_key(name) {
            return;
        }
        let Ok(topics) = Topics::new(&self.settings.root, name) else {
            return;
        };
        info!(instance = name, "The instance's Config names this process; acquired");
        self.instances.insert(
            name.to_owned(),
            Instance {
                name: name.to_owned(),
                topics,
                accepted: None,
                last_seen: None,
                invalid: None,
                want: Want::Broken,
                rt: None,
                build: None,
                ready: None,
                failed_config: None,
                retry_at: None,
                slot: BTreeMap::new(),
                installed: BTreeSet::new(),
                replayed: BTreeSet::new(),
            },
        );
    }

    /// What the instance's latest Config asks. A stop under way not yet unsubscribing ends as the
    /// new want needs: released, or disabled.
    fn set_want(&mut self, name: &str, want: Want) {
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
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
            return Ok(());
        }
        let phase = instance.rt.as_ref().map(|rt| (rt.phase, rt.worker));
        match instance.want.clone() {
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
                if phase.is_none() {
                    self.create_rt(ctx, outputs, name, Phase::Live, LinkStatus::Connecting).await?;
                }
                let Some(instance) = self.instances.get_mut(name) else {
                    return Ok(());
                };
                if instance.ready.as_ref().is_some_and(|r| r.config != config) {
                    if let Some(ready) = instance.ready.take() {
                        drop_off_thread(ready.driver);
                    }
                }
                let worker = instance.rt.as_ref().is_some_and(|rt| rt.worker);
                if worker && instance.accepted.as_ref() == Some(&config) {
                    // Serving it already: a construction under way for another Config is stale.
                    instance.build = None;
                    return Ok(());
                }
                if instance.ready.as_ref().is_some_and(|r| r.config == config) {
                    return self.commit(ctx, outputs, name).await;
                }
                // A Config whose construction failed is built again only when nothing serves, at
                // the reconnect pacing.
                if instance.failed_config.as_ref() == Some(&config) && (worker || instance.retry_at.is_some()) {
                    return Ok(());
                }
                self.ensure_build(name, config);
            }
        }
        Ok(())
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
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        if instance.build.as_ref().is_some_and(|(_, c)| *c == config) {
            return;
        }
        instance.build = Some((id, config.clone()));
        let span = instance.rt.as_ref().map_or_else(|| self.span.clone(), |rt| rt.device.span.clone());
        let device = DeviceConfig::new(name, driver_fields(&config));
        self.spawner.build(self.settings.factory.clone(), device, id, self.settings.factory_timeout, span);
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
            let mut timing = self.settings.overrides.apply(info.timing);
            timing.stable = self.settings.device_stable.unwrap_or(timing.stable);
            match config::check_timing(&timing, &self.settings.mqtt) {
                Ok(()) => Ok((driver, DriverInfo { timing, ..info })),
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
                let wanted = self.instances.get(name).is_some_and(|i| i.want == Want::Serve(config.clone()));
                if !wanted {
                    drop_off_thread(driver);
                    return Ok(());
                }
                if let Some(instance) = self.instances.get_mut(name) {
                    instance.ready = Some(Ready { config, driver, info });
                }
                self.reconcile(ctx, outputs, name).await?;
            }
        }
        Ok(())
    }

    /// With nothing serving the instance, a construction's failure makes it failed:
    /// `Status=unreachable`, retried at the reconnect pacing (multi-device spec §4.3).
    fn mark_failed(&mut self, name: &str) {
        let retry = self.settings.retry;
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        let Some(rt) = instance.rt.as_ref().filter(|rt| rt.phase == Phase::Live && !rt.worker) else {
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
            (Phase::Disabled, _) => self.re_enable(ctx, outputs, name).await?,
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
        outputs.remove(&self.shared, old.id);
        self.create_rt(None, outputs, name, Phase::Live, LinkStatus::Connecting).await?;
        let connected = self.conn.connected;
        let Some(instance) = self.instances.get_mut(name) else {
            return Ok(());
        };
        let Some(rt) = instance.rt.as_mut() else {
            return Ok(());
        };
        rt.read = old.read;
        for (topic, seen) in &instance.slot {
            if let (Inbound::State { target }, Some(_)) = (instance.topics.parse(topic), &seen.via) {
                if let Ok(values) = wire::decode_state(&seen.payload) {
                    rt.device.reports.read_back(&target, values);
                }
            }
        }
        let (id, read) = (rt.id, rt.read);
        if connected && read == Sub::Granted {
            let device = rt.device.clone();
            if let (true, Some(ctx)) = (outputs.announce(id), ctx) {
                session::republish(ctx.shared, ctx.client, &device, device.reports.model()).await;
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
                instance.accepted = Some(ready.config);
                instance.failed_config = None;
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
            let pending = outputs.slot(id).is_some_and(|slot| !slot.batch.is_empty()) || device.reports.has_untaken();
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
                    outputs.remove(&self.shared, old.id);
                }
                if let Then::Release { cleanup: true } = then {
                    // Its own tagged topics, as its read-back last saw them, retracted.
                    let own: Vec<String> =
                        instance.slot.iter().filter(|(_, seen)| seen.via.as_deref() == Some(id.as_str())).map(|(t, _)| t.clone()).collect();
                    if connected {
                        for topic in own {
                            outputs.queue_outbox(Part::Model, Out::new(topic, true, Vec::new(), None, Some(&id)));
                        }
                    } else if !own.is_empty() {
                        warn!(
                            kind = "output_lost_at_session_end",
                            instance = name,
                            lost = own.len(),
                            "A deleted instance's retained topics could not be cleared: the session ended"
                        );
                    }
                }
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
    fn route_instance(&mut self, name: &str, topic: &str, publish: &Publish, received: Instant) {
        let id = self.settings.id.clone();
        let Some(instance) = self.instances.get_mut(name) else {
            return;
        };
        let inbound = instance.topics.parse(topic);
        if matches!(inbound, Inbound::State { .. } | Inbound::Status) {
            observe_slot(instance, &id, topic, &inbound, publish);
            return;
        }
        let Some(rt) = instance.rt.as_ref() else {
            debug!(topic, "A request for an instance with no device; ignored");
            return;
        };
        let failed = rt.phase == Phase::Live && !rt.worker && instance.accepted.is_none() && instance.build.is_none();
        let device = rt.device.clone();
        match rt.phase {
            Phase::Stopping { .. } => terminal(
                &device,
                topic,
                publish,
                "the instance's device is being stopped (its Config changed, or it was disabled, deleted or moved)",
            ),
            Phase::Disabled => terminal(&device, topic, publish, "the instance is disabled"),
            Phase::Live => {
                if let Inbound::Config { target } = &inbound {
                    if publish.payload.is_empty() {
                        instance.installed.remove(target);
                    } else {
                        instance.installed.insert(target.clone());
                    }
                    if publish.retain {
                        instance.replayed.insert(target.clone());
                    }
                }
                // A failed instance refuses target-level writes, as commands (multi-device spec §4.3).
                if failed && matches!(inbound, Inbound::DesiredTarget { .. }) {
                    return terminal(&device, topic, publish, "the instance's device could not be built from its Config");
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
    let vias: Vec<&str> = publish
        .properties
        .as_ref()
        .map(|p| p.user_properties.iter().filter(|(k, _)| k == "via").map(|(_, v)| v.as_str()).collect())
        .unwrap_or_default();
    let via = (vias.len() == 1).then(|| vias[0].to_owned());
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
    if let (Inbound::State { target }, true, Some(_)) = (inbound, publish.retain, &via) {
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

/// The terminal policy of an instance that takes no requests — stopping, or disabled (multi-device
/// spec §4.4 step 1): commands and reads refused, with their replies; target-level Desired refused;
/// property Desired ignored (retained: it replays to a successor); target Configs ignored (they
/// replay too). Nothing is posted to any mailbox.
fn terminal(device: &Device, topic: &str, publish: &Publish, why: &str) {
    let _entered = device.span.enter();
    let payload = &publish.payload[..];
    match device.topics.parse(topic) {
        Inbound::Command => {
            let (target, command) = wire::command_names(payload);
            device.reports.request_error(ErrorReport::new(Reason::Refused, why).target(target.as_deref()).command(&command));
            if let Ok(Some(reply)) = ReplyTo::from_properties(publish.properties.as_ref(), &device.topics) {
                let answer = Answer::Failed { reason: Reason::Refused, error: why.to_owned() };
                device.reports.event(Outbound {
                    topic: Some(reply.topic),
                    payload: reply_payload(target.as_deref(), &command, &answer),
                    correlation: reply.correlation,
                });
            }
            debug!(topic, command = %command, "A command for an instance that takes none; refused");
        }
        Inbound::DesiredTarget { target } => {
            device.reports.request_error(ErrorReport::new(Reason::Refused, why).target(Some(&target)));
            debug!(topic, "A target-level write for an instance that takes none; refused");
        }
        Inbound::DesiredProperty { .. } | Inbound::Config { .. } => {
            debug!(topic, "A retained request for an instance that takes none; ignored (it replays to a successor)");
        }
        Inbound::State { .. } | Inbound::Status | Inbound::Unknown => {}
    }
}

/// What a device stop refuses: a command still waiting for it.
const STOPPED_BEFORE: &str = "the device was stopped before it ran (its Config changed, or the instance was disabled, deleted or moved)";

#[cfg(test)]
mod tests;

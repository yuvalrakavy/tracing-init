//! The device worker (Bridge Runtime spec §5; lifted from mqtt_ac's `coolmaster.rs` — catch-up,
//! `DeviceOutage`, `command_failed`, paced reconnects — and mqtt_lutron's probe accounting).
//!
//! It owns the driver and calls it one operation at a time, each under its deadline: an overrun,
//! a `LinkError` or a helper's `link_lost` is a lost link, never a hang. The link goes
//! `Connecting` → `Proving` (connected, awaiting the first good read or probe) → `Up`, or `Down`;
//! `Down` retries after the pacing interval — at most one connect per `retry`, after a failed
//! connect and after a lost link alike (no-hang 3b: a device that accepts and drops was reconnected
//! to in a loop). Entering `Up` runs the catch-up: the proving read, the outage conflict rule over
//! what was held, then the held state applied. The outage is one episode in the log: INFO
//! `device_connection_lost` on its first failure, DEBUG retries, one WARN `device_unreachable` past
//! the threshold, INFO `device_recovered` once the catch-up is done — never on a bare connect.
//!
//! It waits on nothing but the driver (bounded), its mailbox and its pacing: what it reports goes
//! to [`Reports`], which never waits, so a stalled broker holds up none of its work.
//!
//! The worker runs on a thread of its own, on a single-threaded runtime ([`spawn`]): the driver's
//! futures need not be `Send`, and a driver that blocks its thread holds up only itself — never the
//! MQTT side or the stop.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::{oneshot, watch};
use tracing::{debug, error, info, warn, Instrument, Span};

use crate::driver::{DeviceDriver, DriverInfo, FailureClass, Feedback, LinkError, OpError, PropertyMap, Scope, TargetState, Writes};
use crate::link::{Abandoned, LinkContext, LinkHandle, OpClock};
use crate::mailbox::{Held, Mailbox, ReadKey, Resolution, StateValue, Unanswered, Work};
use crate::replies::{reply_payload, Answer, ReplyTo};
use crate::reports::{LinkStatus, Outbound, Reports};
use crate::wire::{Conflict, ErrorReport, Reason};

/// While the worker writes to a `Writes::Unacknowledged` device without a pause, it still probes at
/// least this often, so what it wrote is proven taken — or the device found gone — within this and
/// the probe's deadline (mqtt_lutron's `BUSY_PROBE`).
pub const BUSY_PROBE: Duration = Duration::from_secs(2);

/// The bound on `disconnect` at shutdown.
pub const DISCONNECT_BOUND: Duration = Duration::from_secs(2);

/// The device's reachability as the worker sees it. The worker outlives every link, so an outage
/// is one episode however many attempts it takes (the logging policy, and the owner's device-down
/// policy).
struct DeviceOutage {
    warn_after: Duration,
    /// A recovered link that lasts this long ends the outage.
    stable: Duration,
    current: Option<Episode>,
}

struct Episode {
    since: Instant,
    attempts: u32,
    warned: bool,
    /// What the catch-ups of the episode applied, refused and dropped, for its recovery line.
    applied: usize,
    refused: u32,
    expired: u32,
    /// When the link now on trial recovered: the outage ends once it has lasted `stable`.
    on_trial: Option<Instant>,
}

impl DeviceOutage {
    fn new(warn_after: Duration, stable: Duration) -> DeviceOutage {
        DeviceOutage { warn_after, stable, current: None }
    }

    /// The link was lost, or an attempt to connect (or prove) failed. A link on trial lost is one
    /// more attempt of its outage.
    fn failed(&mut self, error: &LinkError) {
        let Some(episode) = &mut self.current else {
            self.current =
                Some(Episode { since: Instant::now(), attempts: 1, warned: false, applied: 0, refused: 0, expired: 0, on_trial: None });
            info!(kind = "device_connection_lost", error = %error, "Device unreachable; retrying");
            return;
        };
        episode.on_trial = None;
        episode.attempts += 1;
        let down_for_ms = episode.since.elapsed().as_millis() as u64;
        if !episode.warned && episode.since.elapsed() >= self.warn_after {
            episode.warned = true;
            warn!(kind = "device_unreachable", error = %error, attempts = episode.attempts, down_for_ms, "Device unreachable; still retrying");
        } else {
            debug!(error = %error, attempts = episode.attempts, down_for_ms, "Device still unreachable");
        }
    }

    /// Proven alive, with the state held meanwhile applied. Outside an outage that is all; during
    /// one, the link is on trial until it has lasted `stable` ([`stable`](Self::stable)).
    fn recovered(&mut self, applied: usize, refused: u32, expired: u32) {
        match &mut self.current {
            Some(episode) => {
                episode.applied += applied;
                episode.refused += refused;
                episode.expired += expired;
                episode.on_trial = Some(Instant::now());
                debug!(attempts = episode.attempts, "Device proven alive; the outage ends once the link lasts");
            }
            None => info!(applied, "Connected to the device"),
        }
    }

    /// When the link on trial will have lasted `stable`, if one is on trial.
    fn stable_at(&self) -> Option<Instant> {
        self.current.as_ref().and_then(|e| e.on_trial).map(|at| at + self.stable)
    }

    /// The link on trial has lasted: the outage is over — one INFO with its length, up to that
    /// link's recovery, and what its catch-ups did.
    fn stable(&mut self) {
        let Some(on_trial) = self.current.as_ref().and_then(|e| e.on_trial) else {
            return;
        };
        if on_trial.elapsed() < self.stable {
            return;
        }
        if let Some(episode) = self.current.take() {
            info!(
                kind = "device_recovered",
                down_for_ms = on_trial.saturating_duration_since(episode.since).as_millis() as u64,
                attempts = episode.attempts,
                applied = episode.applied,
                refused = episode.refused,
                expired = episode.expired,
                "Device reachable again"
            );
        }
    }
}

/// One driver operation, under `bound` and the link's `link_lost`: an overrun or a lost link is a
/// `LinkError`. An operation is not started on a link already lost (review R6). One admitted while
/// the link was live and completed wins over a loss noticed at the same moment — its result is what
/// the device did; the loss is seen at the worker's next step.
async fn op<T>(
    handle: &LinkHandle,
    bound: Duration,
    what: &str,
    operation: impl Future<Output = Result<T, OpError>>,
) -> Result<T, OpError> {
    if let Some(lost) = handle.loss() {
        return Err(OpError::Link(lost));
    }
    let deadline = Instant::now() + bound;
    handle.set_deadline(Some(deadline));
    // WAIT: device-op
    let result = tokio::select! {
        biased;
        done = tokio::time::timeout_at(deadline.into(), crate::link::under_deadline(deadline, operation)) => match done {
            Ok(result) => result,
            Err(_) => Err(OpError::Link(LinkError::new(format!("{what} missed its deadline ({} ms)", bound.as_millis())))),
        },
        lost = handle.lost() => Err(OpError::Link(lost)),
    };
    handle.set_deadline(None);
    result
}

/// Sleep until `at`, or for good when there is none.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        // WAIT: worker-up
        None => std::future::pending::<()>().await,
    }
}

/// How the outage conflict rule decides a checked value (Store kit spec §4.5): against the
/// device's current value (`None`: the device did not say) and the last value the bridge saw
/// before the outage (`None`: none, a first install). The first row that holds decides:
/// 1. the current value known and the requested one → nothing to write;
/// 2. no before value → applied: nothing can have changed;
/// 3. the current value unknown → under `DeviceWins` not applied on a guess (review M6), under
///    `RequestWins` applied;
/// 4. changed since before → `DeviceWins` supersedes, `RequestWins` applies;
/// 5. otherwise → applied.
pub(crate) fn resolve(value: &StateValue, current: Option<&Value>, before: Option<&Value>) -> Resolution {
    if current.is_some_and(|current| same_value(current, &value.value)) {
        return Resolution::Satisfied;
    }
    let Some(before) = before else {
        return Resolution::Apply;
    };
    let Some(current) = current else {
        return match value.conflict {
            Conflict::DeviceWins => Resolution::Unknown,
            Conflict::RequestWins => Resolution::Apply,
        };
    };
    if same_value(before, current) {
        return Resolution::Apply;
    }
    match value.conflict {
        Conflict::DeviceWins => Resolution::Superseded,
        Conflict::RequestWins => Resolution::Apply,
    }
}

/// Whether two JSON values are the same value: numbers by value — `24` and `24.0` are one, as a
/// device may report what was asked written another way — the rest as written. Two integers
/// compare exactly; only a float goes through `f64`.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (integer(x), integer(y)) {
            (Some(x), Some(y)) => x == y,
            _ => x == y || x.as_f64().zip(y.as_f64()).is_some_and(|(x, y)| x == y),
        },
        _ => a == b,
    }
}

/// A JSON integer, exactly.
fn integer(n: &serde_json::Number) -> Option<i128> {
    n.as_i64().map(i128::from).or_else(|| n.as_u64().map(i128::from))
}

/// Values by target and property, for the conflict rule.
type Values = HashMap<(String, String), Value>;

/// The conflict rule's "before" values, each with when it was taken.
struct Befores {
    /// What the bridge knew of the device when its outage began (the read-back before any).
    device: (Instant, Values),
    /// What the bridge knew when it stopped listening to the broker (the read-back in the
    /// process's first session).
    broker: (Instant, Values),
}

impl Befores {
    /// A checked value's "before" value (review B1): a replayed one's is what the bridge knew when
    /// it stopped listening; a held one's, what it knew of the device when its outage began; one
    /// both replayed and held takes the older of the two first, and the other where the older
    /// lacks the key.
    fn of<'a>(&'a self, value: &'a StateValue) -> Option<&'a Value> {
        let key = (value.target.clone(), value.property.clone());
        // A replay's basis was captured when it arrived (re-gate R2); one without falls back to
        // the snapshot now.
        let broker = value.basis.as_ref().map(|b| (b.at, b.before.as_ref())).unwrap_or_else(|| (self.broker.0, self.broker.1.get(&key)));
        let mut sources: Vec<(Instant, Option<&Value>)> = Vec::new();
        if value.check.replayed {
            sources.push(broker);
        }
        if value.check.held {
            sources.push((self.device.0, self.device.1.get(&key)));
        }
        sources.sort_by_key(|(at, _)| *at);
        sources.into_iter().find_map(|(_, before)| before)
    }
}

fn index_map(map: &BTreeMap<String, PropertyMap>, into: &mut Values) {
    for (target, values) in map {
        for (property, value) in values {
            into.insert((target.clone(), property.clone()), value.clone());
        }
    }
}

/// What the Up loop does next.
enum Step {
    /// The link on trial has lasted: the outage ends.
    Stable,
    Probe,
    Poll,
    Work(Work),
}

/// See the module documentation.
pub(crate) struct Worker<D: DeviceDriver> {
    pub(crate) driver: D,
    info: DriverInfo,
    instance: String,
    mailbox: Arc<Mailbox>,
    reports: Arc<Reports>,
    abandoned: Arc<Abandoned>,
    /// The running operation's deadline, for the watchdog on the MQTT side.
    clock: Arc<OpClock>,
    outage: DeviceOutage,
    generation: u64,
    /// When the last connect began: the next waits out `retry` from it.
    last_attempt: Option<Instant>,
    /// What the bridge knew of the device when a link that had recovered (its catch-up done) was
    /// last lost, and when: the conflict rule's "before" values for a device outage. Before any,
    /// the read-back stands for it. A link lost before its catch-up is done leaves it as it is
    /// (review M1: retaken from the proving read's values, it forgot a change at the wall).
    before: Option<(Instant, BTreeMap<String, PropertyMap>)>,
    /// Whether the current link has recovered: proven, and its catch-up done.
    recovered: bool,
    /// The config `configure` last took for each target on this link: one identical is not handed
    /// over again while the link stays up (every reconnect replays the retained configs; review
    /// MINOR 5). Forgotten when the link is lost — the device may have lost its setup with it.
    configured: HashMap<String, Option<Value>>,
    /// Whether the driver's `Unsupported` probe was said (once per process).
    unsupported_probe_said: bool,
    /// The stop, once the worker runs on its thread: asked, no more work is taken.
    stopping: Option<watch::Receiver<bool>>,
}

impl<D: DeviceDriver> Worker<D> {
    pub(crate) fn new(driver: D, info: DriverInfo, instance: &str, mailbox: Arc<Mailbox>, reports: Arc<Reports>) -> Worker<D> {
        Worker {
            driver,
            outage: DeviceOutage::new(info.timing.outage_warn, info.timing.stable),
            info,
            instance: instance.to_owned(),
            mailbox,
            reports,
            abandoned: Arc::new(Abandoned::default()),
            clock: Arc::new(OpClock::new()),
            generation: 0,
            last_attempt: None,
            before: None,
            recovered: false,
            configured: HashMap::new(),
            unsupported_probe_said: false,
            stopping: None,
        }
    }

    /// Whether the stop has been asked: no more work is taken.
    fn stopping(&self) -> bool {
        self.stopping.as_ref().is_some_and(|stop| *stop.borrow())
    }

    /// The runtime assumes what it wrote to a target with assumed feedback: proven at once for an
    /// acknowledged write, unproven until a probe for an unacknowledged one (re-gate R3).
    fn assume(&self, target: &str, values: PropertyMap) {
        match self.info.writes {
            Writes::Acknowledged => self.reports.state(target, values, true),
            Writes::Unacknowledged => self.reports.assume_unproven(target, values),
        }
    }

    /// A probe or read proved every write made before it: the unproven ones, and their assumptions.
    fn proven(&self) {
        let proven = self.mailbox.proven();
        self.reports.assumptions_proven(&proven);
    }

    /// The clock the watchdog on the MQTT side reads.
    pub(crate) fn clock(&self) -> Arc<OpClock> {
        self.clock.clone()
    }

    /// The blocking calls' count, which the watchdog reads too: a cap that stays full is a dead
    /// worker.
    pub(crate) fn abandoned(&self) -> Arc<Abandoned> {
        self.abandoned.clone()
    }

    /// The worker's life: link after link. It never returns; the stop cancels it.
    pub(crate) async fn run(&mut self) {
        self.reports.status(LinkStatus::Connecting);
        loop {
            if let Some(at) = self.last_attempt {
                tokio::time::sleep_until((at + self.info.timing.retry).into()).await;
            }
            self.last_attempt = Some(Instant::now());
            self.generation += 1;
            let mut ctx =
                LinkContext::new(self.generation, &self.instance, self.reports.clone(), self.abandoned.clone(), self.clock.clone());
            let handle = ctx.handle();
            let error = self.link(&mut ctx, &handle).await;
            self.down(ctx, error).await;
        }
    }

    /// One link, from its connect until it is lost.
    async fn link(&mut self, ctx: &mut LinkContext, handle: &LinkHandle) -> LinkError {
        let timing = self.info.timing;
        let driver = &mut self.driver;
        // WAIT: device-op
        if let Err(e) = op(handle, timing.connect, "connect", async { driver.connect(ctx).await.map_err(OpError::Link) }).await {
            return link_error(e);
        }
        let read = match self.prove(handle).await {
            Ok(read) => read,
            Err(e) => return e,
        };
        let refused = self.mailbox.connected();
        self.reports.status(LinkStatus::Connected);
        let applied = match self.catch_up(handle, read).await {
            Ok(applied) => applied,
            Err(e) => return e,
        };
        self.recovered = true;
        self.outage.recovered(applied, refused, self.mailbox.take_expired());
        self.serve(handle).await
    }

    /// Proof of life: the first good read — or, for a driver that cannot read, probe. A device
    /// that answers with something unusable has answered: that proves it alive too.
    async fn prove(&mut self, handle: &LinkHandle) -> Result<Option<Vec<TargetState>>, LinkError> {
        let timing = self.info.timing;
        // WAIT: device-op
        match op(handle, timing.operation, "read_state", self.driver.read_state(Scope::All)).await {
            Ok(states) => {
                self.proven();
                self.fold(&states);
                Ok(Some(states))
            }
            Err(OpError::Link(e)) => Err(e),
            // WAIT: device-op
            Err(OpError::Unsupported(_)) => match op(handle, timing.probe_deadline, "probe", self.driver.probe()).await {
                Err(OpError::Link(e)) => Err(e),
                Err(OpError::Unsupported(message)) => {
                    self.unsupported_probe(&message);
                    Ok(None)
                }
                _ => Ok(None),
            },
            Err(other) => {
                debug!(error = %other, "The device answered the proving read with something unusable");
                Ok(None)
            }
        }
    }

    /// The conflict rule's "before" values, for each way a value can be checked (Store kit spec
    /// §4.5): see [`Befores::of`].
    fn befores(&self) -> Befores {
        let (device_at, device) = self.before.clone().unwrap_or_else(|| self.reports.read_back_before());
        let (broker_at, broker) = self.reports.broker_before();
        let (mut device_values, mut broker_values) = (HashMap::new(), HashMap::new());
        index_map(&device, &mut device_values);
        index_map(&broker, &mut broker_values);
        Befores { device: (device_at, device_values), broker: (broker_at, broker_values) }
    }

    /// Entering `Up`: what was held while the device was down, decided by the conflict rule
    /// against the proving read — a target it did not give is read on its own — then applied a
    /// target at a time; then, for an unacknowledged device, a probe proves it taken. Returns how
    /// many values were written.
    async fn catch_up(&mut self, handle: &LinkHandle, read: Option<Vec<TargetState>>) -> Result<usize, LinkError> {
        let mut proving: BTreeMap<String, PropertyMap> = BTreeMap::new();
        for state in read.into_iter().flatten() {
            proving.entry(state.target).or_default().extend(state.values);
        }
        let mut applied = 0;
        // A target's setup first, then the held values.
        while let Some((target, config)) = self.mailbox.take_held_config() {
            self.configure(handle, &target, config).await?;
        }
        // Nothing more is taken once the stop is asked (re-gate 4 minor 1).
        while !self.stopping() {
            match self.mailbox.take_held() {
                None => break,
                Some(Held::Config { target, config }) => self.configure(handle, &target, config).await?,
                Some(Held::Values(held)) => {
                    let apply = self.decide(handle, &held.target, held.values, &proving).await?;
                    applied += self.write_values(handle, &held.target, apply).await?;
                }
            }
        }
        if self.info.writes == Writes::Unacknowledged && self.mailbox.unproven_since().is_some() {
            self.probe(handle).await?;
        }
        Ok(applied)
    }

    /// Requests a change at the device won (`unknown`: whose current value is unknown, under
    /// `DeviceWins`): dropped, each reported on `Error`.
    fn superseded(&self, values: &[StateValue], unknown: bool) {
        let message = if unknown {
            "the device's current value is unknown; under DeviceWins a request is not applied on a guess"
        } else {
            "the device was changed during the outage; the change at the device wins"
        };
        for value in values {
            info!(target = %value.target, property = %value.property, unknown, "A held request was superseded by a change at the device");
            self.reports.error(ErrorReport::new(Reason::Superseded, message).target(Some(&value.target)).property(&value.property));
        }
    }

    /// Fold a read into the retained model.
    fn fold(&self, states: &[TargetState]) {
        for state in states {
            self.reports.state(&state.target, state.values.clone(), false);
        }
    }

    /// Apply one target's values together. A failure of the request alone is reported and the
    /// request passed over; only a lost link is an error.
    async fn write_values(&mut self, handle: &LinkHandle, target: &str, values: Vec<StateValue>) -> Result<usize, LinkError> {
        if values.is_empty() {
            self.mailbox.failed();
            return Ok(0);
        }
        let map: PropertyMap = values.iter().map(|v| (v.property.clone(), v.value.clone())).collect();
        let feedback = self.mailbox.feedback(target, self.info.feedback);
        let reported_before = self.reports.reports_for(target);
        let timing = self.info.timing;
        match op(handle, timing.operation, "apply", self.driver.apply(target, &map)).await {
            Ok(()) => {
                self.mailbox.applied(self.info.writes == Writes::Unacknowledged);
                if self.info.writes == Writes::Acknowledged {
                    // What the device acknowledged is what the bridge knows (the lead's F2).
                    self.reports.written(target, &map, reported_before);
                }
                if feedback == Feedback::Assumed && self.reports.reports_for(target) == reported_before {
                    self.assume(target, map);
                }
                Ok(values.len())
            }
            Err(OpError::Link(e)) => Err(e),
            Err(OpError::Partial(failures)) => {
                // Every value it does not name was applied; those it names failed, each said.
                let failed = |v: &StateValue| failures.iter().any(|f| f.property == v.property);
                self.mailbox.retain_in_flight(|v| !failed(v));
                self.mailbox.applied(self.info.writes == Writes::Unacknowledged);
                // A request that failed did not take its key: the restore's, if it left it (re-gate
                // 5 minor 1).
                let unapplied: Vec<StateValue> = values.iter().filter(|v| failed(v)).cloned().collect();
                self.mailbox.restore_deferred(&unapplied);
                let applied: PropertyMap = map
                    .iter()
                    .filter(|(property, _)| !failures.iter().any(|f| f.property == **property))
                    .map(|(property, value)| (property.clone(), value.clone()))
                    .collect();
                let count = applied.len();
                if self.info.writes == Writes::Acknowledged {
                    self.reports.written(target, &applied, reported_before);
                }
                if feedback == Feedback::Assumed && self.reports.reports_for(target) == reported_before && !applied.is_empty() {
                    self.assume(target, applied);
                }
                for failure in &failures {
                    let reason = match failure.class {
                        FailureClass::Rejected => Reason::Rejected,
                        FailureClass::Unusable => Reason::Unusable,
                        FailureClass::Unsupported => Reason::Unsupported,
                    };
                    if reason == Reason::Rejected {
                        warn!(kind = "command_rejected", target, property = %failure.property, error = %failure.error, "The device rejected a property of a request");
                    } else {
                        info!(target, property = %failure.property, error = %failure.error, ?reason, "A property of a request failed");
                    }
                    let restored = values.iter().any(|v| v.restored && v.property == failure.property);
                    self.request_failed(
                        restored,
                        ErrorReport::new(reason, failure.error.clone()).target(Some(target)).property(&failure.property),
                    );
                }
                Ok(count)
            }
            Err(failure) => {
                self.mailbox.failed();
                self.mailbox.restore_deferred(&values);
                let properties: Vec<&str> = values.iter().map(|v| v.property.as_str()).collect();
                let (reason, message) = classify(&failure);
                match reason {
                    Reason::Rejected => {
                        warn!(kind = "command_rejected", target, ?properties, error = message, "The device rejected a request")
                    }
                    Reason::Unsupported => {
                        info!(target, ?properties, error = message, "A request named something the device does not support")
                    }
                    _ => debug!(target, ?properties, error = message, "The device's answer to a request was unusable; passed over"),
                }
                for value in &values {
                    self.request_failed(value.restored, ErrorReport::new(reason, message).target(Some(target)).property(&value.property));
                }
                Ok(0)
            }
        }
    }

    /// A value's failure: on `Error` for a request; for a restored value, which no one requested, a
    /// WARN `restore_failed` (re-gate 4 minor 10).
    fn request_failed(&self, restored: bool, report: ErrorReport) {
        if restored {
            warn!(
                kind = "restore_failed",
                target = report.target.as_deref(),
                property = report.property.as_deref(),
                error = %report.error,
                "A restored value failed at the device"
            );
        } else {
            self.reports.error(report);
        }
    }

    /// Hand a target its setup — unless `configure` took the same one on this link already.
    async fn configure(&mut self, handle: &LinkHandle, target: &str, config: Option<Value>) -> Result<(), LinkError> {
        if self.configured.get(target) == Some(&config) {
            debug!(target, "A config identical to the one this link has already taken; not handed over again");
            self.mailbox.configured(target, config.as_ref());
            return Ok(());
        }
        let timing = self.info.timing;
        // WAIT: device-op
        match op(handle, timing.operation, "configure", self.driver.configure(target, config.as_ref())).await {
            Ok(()) => {
                self.mailbox.configured(target, config.as_ref());
                self.configured.insert(target.to_owned(), config);
                Ok(())
            }
            Err(OpError::Link(e)) => Err(e),
            Err(failure) => {
                self.mailbox.failed();
                let (reason, message) = classify(&failure);
                if reason == Reason::Rejected {
                    warn!(kind = "command_rejected", target, error = message, "The device rejected a target's configuration");
                }
                self.reports.error(ErrorReport::new(reason, format!("configuration: {message}")).target(Some(target)));
                Ok(())
            }
        }
    }

    /// A probe: proves every unproven write, or finds the link lost.
    async fn probe(&mut self, handle: &LinkHandle) -> Result<(), LinkError> {
        let timing = self.info.timing;
        // WAIT: device-op
        let probed = op(handle, timing.probe_deadline, "probe", self.driver.probe()).await;
        match probed {
            Err(OpError::Link(e)) => Err(e),
            // An answer, even an unusable one, was written after every unproven write. An
            // `Unsupported` one sent nothing, so it proves nothing — but a driver that cannot probe
            // leaves nothing better to wait for, and waiting would be a probe hot loop: its writes
            // are taken as proven, and the contract violation said once (review C11b).
            other => {
                if let Err(OpError::Unsupported(message)) = other {
                    self.unsupported_probe(&message);
                }
                self.proven();
                Ok(())
            }
        }
    }

    /// A probe answered `Unsupported`: for a driver with unacknowledged writes, which must prove
    /// them, a contract violation — ERROR `driver_contract_violation`, once per process.
    fn unsupported_probe(&mut self, message: &str) {
        if self.info.writes == Writes::Unacknowledged && !std::mem::replace(&mut self.unsupported_probe_said, true) {
            error!(
                kind = "driver_contract_violation",
                error = message,
                "A driver with unacknowledged writes cannot probe: its writes are taken as proven without proof"
            );
        }
    }

    /// When the next probe falls due, for an unacknowledged device: at once when something
    /// written waits for proof and nothing else waits, `BUSY_PROBE` after the oldest unproven write
    /// while work waits, else once the device has been quiet for the probe interval.
    fn probe_due(&self, last_heard: Instant) -> Option<Instant> {
        if self.info.writes != Writes::Unacknowledged {
            return None;
        }
        Some(match self.mailbox.unproven_since() {
            Some(_) if !self.mailbox.waiting() => Instant::now(),
            Some(since) => since + BUSY_PROBE.min(self.info.timing.probe_interval),
            None => last_heard + self.info.timing.probe_interval,
        })
    }

    /// `Up`: the mailbox, polls and probes, until the link is lost. A poll is due an interval after
    /// the last one ended, and one waiting request is served after each poll.
    async fn serve(&mut self, handle: &LinkHandle) -> LinkError {
        let timing = self.info.timing;
        let mut last_read = Instant::now();
        let mut last_heard = Instant::now();
        let mailbox = self.mailbox.clone();
        loop {
            let poll_at = timing.poll.map(|p| last_read + p);
            let probe_at = self.probe_due(last_heard);
            let stable_at = self.outage.stable_at();
            // WAIT: worker-up
            let step = tokio::select! {
                biased;
                lost = handle.lost() => return lost,
                () = sleep_until(stable_at) => Step::Stable,
                () = sleep_until(probe_at) => Step::Probe,
                () = sleep_until(poll_at) => Step::Poll,
                work = mailbox.take(), if !self.stopping() => Step::Work(work),
            };
            let outcome = match step {
                Step::Stable => {
                    self.outage.stable();
                    Ok(())
                }
                Step::Probe => self.probe(handle).await.map(|()| last_heard = Instant::now()),
                Step::Poll => {
                    // WAIT: device-op
                    let polled = match op(handle, timing.operation, "read_state", self.driver.read_state(Scope::All)).await {
                        Ok(states) => {
                            last_heard = Instant::now();
                            self.proven();
                            self.fold(&states);
                            Ok(())
                        }
                        Err(OpError::Link(e)) => Err(e),
                        Err(other) => {
                            debug!(error = %other, "A poll's answer was unusable");
                            Ok(())
                        }
                    };
                    // The next poll is due an interval after this one ended, and the mailbox is
                    // served before it, whatever is due: a poll slower than its interval would
                    // otherwise be due at once, every time, and starve every write (review C4).
                    last_read = Instant::now();
                    // A poll that lost the link takes nothing off the mailbox: what waits there is
                    // the disconnect's to hold or refuse, and answer (review R4).
                    match polled {
                        Ok(()) => match (!self.stopping()).then(|| self.mailbox.try_take()).flatten() {
                            Some(work) => self.work(handle, work).await,
                            None => Ok(()),
                        },
                        Err(e) => Err(e),
                    }
                }
                Step::Work(work) => self.work(handle, work).await,
            };
            if let Err(e) = outcome {
                return e;
            }
        }
    }

    async fn work(&mut self, handle: &LinkHandle, work: Work) -> Result<(), LinkError> {
        let timing = self.info.timing;
        match work {
            Work::Apply { target, values } => {
                let values = self.decide(handle, &target, values, &BTreeMap::new()).await?;
                self.write_values(handle, &target, values).await.map(drop)
            }
            Work::Configure { target, config } => self.configure(handle, &target, config).await,
            Work::Expired { target, call, reply } => {
                let message = "the command reached its expiry while it waited for the device; not executed";
                debug!(target = target.as_deref(), command = %call.command, "A command expired while it waited");
                report_refused(
                    &self.reports,
                    vec![Unanswered { ran: false, target, command: call.command, momentary: true, replies: reply.into_iter().collect() }],
                    message,
                );
                Ok(())
            }
            Work::Execute { target, call, reply } => {
                let result = op(handle, timing.operation, "execute", self.driver.execute(target.as_deref(), &call)).await;
                self.answer_once(target.as_deref(), &call.command, result, reply.into_iter().collect())
            }
            Work::ReadExpired { expired } => {
                report_refused(&self.reports, expired, READ_EXPIRED);
                Ok(())
            }
            Work::Read { key: ReadKey::Refresh(target), replies, expired, .. } => {
                report_refused(&self.reports, expired, READ_EXPIRED);
                let scope = target.clone().map_or(Scope::All, Scope::Target);
                // WAIT: device-op
                let result = op(handle, timing.operation, "read_state", self.driver.read_state(scope)).await.map(|states| {
                    self.proven();
                    self.fold(&states);
                    None
                });
                // A failure goes to Error too, as every command's does (review C13).
                self.answer_once(target.as_deref(), "refresh", result, replies)
            }
            Work::Read { key: ReadKey::Command { target, command }, call, replies, expired } => {
                report_refused(&self.reports, expired, READ_EXPIRED);
                let call = call.unwrap_or_else(|| crate::driver::CommandCall { command: command.clone(), params: PropertyMap::new() });
                let result = op(handle, timing.operation, "execute", self.driver.execute(target.as_deref(), &call)).await;
                self.answer_once(target.as_deref(), &command, result, replies)
            }
        }
    }

    /// The outage conflict rule over a target's values (Store kit spec §4.5); returns what to
    /// apply, the rest out of the in-flight set and reported. A value not checked is applied as it
    /// is. The current values: for a target with assumed feedback, what the bridge believes — the
    /// model (review MINOR 7: a device that cannot be read gave nothing, and a replay was sent
    /// again); otherwise the proving read's, or — a target it did not give — a read of the target
    /// on its own (review M6). After that read's wait, what was retracted, replaced or expired
    /// meanwhile is dropped (review C8).
    async fn decide(
        &mut self,
        handle: &LinkHandle,
        target: &str,
        values: Vec<StateValue>,
        proving: &BTreeMap<String, PropertyMap>,
    ) -> Result<Vec<StateValue>, LinkError> {
        let values = self.restorable(target, values);
        if !values.iter().any(|v| v.check.any()) {
            return Ok(values);
        }
        let timing = self.info.timing;
        let (current, values) = if self.mailbox.feedback(target, self.info.feedback) == Feedback::Assumed {
            (self.reports.values_of(target), values)
        } else if let Some(read) = proving.get(target) {
            (Some(read.clone()), values)
        } else {
            // WAIT: device-op
            let current = match op(handle, timing.operation, "read_state", self.driver.read_state(Scope::Target(target.to_owned()))).await {
                Ok(states) => {
                    self.proven();
                    self.fold(&states);
                    let mut current: Option<PropertyMap> = None;
                    for state in states.into_iter().filter(|s| s.target == target) {
                        current.get_or_insert_with(PropertyMap::new).extend(state.values);
                    }
                    current
                }
                Err(OpError::Link(e)) => return Err(e),
                Err(other) => {
                    debug!(target, error = %other, "The device's answer to the conflict read was unusable; its values are unknown");
                    None
                }
            };
            (current, self.mailbox.revalidate(values))
        };
        let befores = self.befores();
        let (mut apply, mut superseded, mut unknown, mut satisfied) = (Vec::new(), Vec::new(), Vec::new(), 0);
        for value in values {
            if !value.check.any() {
                // A live value, taken with a checked one: applied as it is (review MINOR 3).
                apply.push(value);
                continue;
            }
            match resolve(&value, current.as_ref().and_then(|c| c.get(&value.property)), befores.of(&value)) {
                Resolution::Apply => apply.push(value),
                Resolution::Satisfied => satisfied += 1,
                Resolution::Superseded => superseded.push(value),
                Resolution::Unknown => unknown.push(value),
            }
        }
        if satisfied > 0 {
            debug!(target, satisfied, "Checked requests the device already has");
        }
        self.mailbox.retain_in_flight(|v| apply.iter().any(|a| a.target == v.target && a.property == v.property));
        self.superseded(&superseded, false);
        self.superseded(&unknown, true);
        let restored = self.mailbox.restore_deferred(&unknown);
        if restored > 0 {
            info!(target, values = restored, "Keys whose request was superseded on an unknown value are restored");
        }
        Ok(apply)
    }

    /// A target's values less the restored ones, unless its feedback — its config's, taken before
    /// its values — is still `Assumed`: a restore is for a target the bridge cannot read, not one
    /// that now reports (re-gate 4 F2: a target reported since was restored over the device's own
    /// value). What is dropped leaves the in-flight set.
    fn restorable(&self, target: &str, values: Vec<StateValue>) -> Vec<StateValue> {
        if !values.iter().any(|v| v.restored) || self.mailbox.feedback(target, self.info.feedback) == Feedback::Assumed {
            return values;
        }
        let (kept, dropped): (Vec<StateValue>, Vec<StateValue>) = values.into_iter().partition(|v| !v.restored);
        info!(target, values = dropped.len(), "A target no longer assumed is not restored");
        self.mailbox.retain_in_flight(|v| kept.contains(v));
        kept
    }

    /// The outcome of the command the worker took: answered if it is still the worker's to answer
    /// ([`Mailbox::command_done_then`]) — exactly one answer per command, queued in the same step
    /// that ends its flight, so a stop's flush cannot refuse what waits and take the reports
    /// between the two (re-gate 5 minor 4) — and a lost link returned either way.
    fn answer_once(
        &self,
        target: Option<&str>,
        command: &str,
        result: Result<Option<Value>, OpError>,
        replies: Vec<ReplyTo>,
    ) -> Result<(), LinkError> {
        let mut result = Some(result);
        let mut outcome = None;
        let answered = self.mailbox.command_done_then(|| {
            if let Some(result) = result.take() {
                outcome = Some(self.answered(target, command, result, replies, true));
            }
        });
        if let (true, Some(outcome)) = (answered, outcome) {
            return outcome;
        }
        let Some(result) = result else {
            return Ok(());
        };
        debug!(target, command, "A command answered as cut off finished after all; not answered again");
        match result {
            Err(OpError::Link(e)) => Err(e),
            _ => Ok(()),
        }
    }

    /// A command's outcome: replied to, a failure reported on `Error` (`error_lane`), and a lost
    /// link returned.
    fn answered(
        &self,
        target: Option<&str>,
        command: &str,
        result: Result<Option<Value>, OpError>,
        replies: Vec<ReplyTo>,
        error_lane: bool,
    ) -> Result<(), LinkError> {
        let (answer, lost) = match result {
            Ok(value) => (Answer::Done(value), None),
            Err(failure) => {
                let (reason, message) = match &failure {
                    OpError::Link(_) => (Reason::LinkLost, "the device link was lost while the command ran; it is not retried"),
                    other => classify(other),
                };
                if reason == Reason::Rejected {
                    warn!(kind = "command_rejected", target, command, error = message, "The device rejected a command");
                }
                if error_lane {
                    self.reports.error(ErrorReport::new(reason, message).target(target).command(command));
                }
                let answer = Answer::Failed { reason, error: message.to_owned() };
                let lost = match failure {
                    OpError::Link(e) => Some(e),
                    _ => None,
                };
                (answer, lost)
            }
        };
        for reply in replies {
            self.reports.event(Outbound {
                topic: Some(reply.topic),
                payload: reply_payload(target, command, &answer),
                correlation: reply.correlation,
            });
        }
        match lost {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// The link is lost: its helpers cancelled, the driver told (bounded), what the device may not
    /// have taken held again, the momentary commands waiting refused, the outage episode advanced.
    async fn down(&mut self, ctx: LinkContext, error: LinkError) {
        drop(ctx);
        self.configured.clear();
        if std::mem::take(&mut self.recovered) {
            self.before = Some((Instant::now(), self.reports.snapshot()));
        }
        // Watched like an operation: a disconnect that blocks the thread is a dead worker too. Its
        // blocking calls are held to its bound, as an operation's.
        let deadline = Instant::now() + self.info.timing.operation;
        self.clock.set(Some(deadline));
        // WAIT: device-disconnect, device-disconnect
        let _ = tokio::time::timeout_at(deadline.into(), crate::link::under_deadline(deadline, self.driver.disconnect())).await;
        self.clock.set(None);
        let unanswered = self.mailbox.disconnected();
        report_refused(&self.reports, unanswered, LINK_LOST_BEFORE);
        self.reports.status(LinkStatus::Unreachable);
        self.outage.failed(&error);
    }
}

/// What a disconnect refuses: a command the device never took.
pub(crate) const LINK_LOST_BEFORE: &str = "the device link was lost before it ran";

/// What a command the worker was running, and was cut off from answering, is told.
const CUT_OFF: &str = "the command was cut off while it ran: the device may or may not have done it";

/// What a read's caller past its deadline is told.
const READ_EXPIRED: &str = "the command reached its expiry while it waited for the device; not executed";

/// What a disconnect refused, or what expired: each command on `Error`, and every reply waited for.
pub(crate) fn report_refused(reports: &Reports, unanswered: Vec<Unanswered>, message: &str) {
    for u in unanswered {
        // One the worker was running was cut off, not refused before it ran (re-gate 3 S3).
        let message = if u.ran { CUT_OFF } else { message };
        // A read refused is a failure too: on Error, as every command's (review C13).
        reports.error(ErrorReport::new(Reason::Refused, message).target(u.target.as_deref()).command(&u.command));
        let answer = Answer::Failed { reason: Reason::Refused, error: message.to_owned() };
        for reply in u.replies {
            reports.event(Outbound {
                topic: Some(reply.topic),
                payload: reply_payload(u.target.as_deref(), &u.command, &answer),
                correlation: reply.correlation,
            });
        }
    }
}

/// A connect's error as the link's.
fn link_error(e: OpError) -> LinkError {
    match e {
        OpError::Link(e) => e,
        other => LinkError::new(other.to_string()),
    }
}

/// A request's failure: its `Error` reason and message.
fn classify(failure: &OpError) -> (Reason, &str) {
    match failure {
        OpError::Rejected(m) => (Reason::Rejected, m),
        OpError::Unusable(m) => (Reason::Unusable, m),
        OpError::Unsupported(m) => (Reason::Unsupported, m),
        OpError::Link(e) => (Reason::LinkLost, e.reason()),
        // A partial outcome means something only for `apply`; elsewhere it is a refusal.
        OpError::Partial(failures) => (Reason::Rejected, failures.first().map_or("a partial outcome", |f| f.error.as_str())),
    }
}

/// How the worker thread ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerEnd {
    /// Asked to stop.
    Stopped,
    /// A panic (the driver's, or the runtime's), or its runtime could not be built.
    Died(String),
}

/// Start the worker on a thread of its own with a single-threaded runtime, until `stop` turns
/// true: then the driver is disconnected (within [`DISCONNECT_BOUND`]) and the thread ends. Its
/// end is sent on the returned channel — also a panic's.
pub(crate) fn spawn<D: DeviceDriver>(
    worker: Worker<D>,
    mut stop: watch::Receiver<bool>,
    span: Span,
) -> std::io::Result<oneshot::Receiver<WorkerEnd>> {
    let (tx, rx) = oneshot::channel();
    let clock = worker.clock();
    std::thread::Builder::new().name("bridge-worker".into()).spawn(move || {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("its runtime: {e}"))?;
            let mut worker = worker;
            // Helpers run here too, on this runtime's local set: they need not be `Send`.
            let local = tokio::task::LocalSet::new();
            // WAIT: worker-thread
            local.block_on(
                &runtime,
                async {
                    // The heartbeat the watchdog reads: a blocked thread stops it (re-gate R1).
                    tokio::task::spawn_local(heartbeat(clock));
                    // The stop first: once it is asked, nothing more is started (re-gate 4 minor 1).
                    worker.stopping = Some(stop.clone());
                    // WAIT: worker-stop
                    tokio::select! {
                        biased;
                        _ = stop.wait_for(|stop| *stop) => {}
                        () = worker.run() => {}
                    }
                    // WAIT: device-disconnect
                    let _ = tokio::time::timeout(DISCONNECT_BOUND, worker.driver.disconnect()).await;
                }
                .instrument(span),
            );
            runtime.shutdown_timeout(Duration::from_millis(200));
            Ok::<(), String>(())
        }));
        let end = match ran {
            Ok(Ok(())) => WorkerEnd::Stopped,
            Ok(Err(e)) => WorkerEnd::Died(e),
            Err(panic) => WorkerEnd::Died(panic_message(&panic)),
        };
        let _ = tx.send(end);
    })?;
    Ok(rx)
}

/// How often the worker's runtime says it runs.
pub(crate) const HEARTBEAT_TICK: Duration = Duration::from_millis(250);

/// The worker runtime's heartbeat, until the runtime ends: a ticker on the worker's own thread, so
/// any block of the thread — a helper's synchronous read, a driver's sleep — stops it.
async fn heartbeat(clock: Arc<OpClock>) {
    loop {
        clock.beat();
        tokio::time::sleep(HEARTBEAT_TICK).await;
    }
}

/// A panic's payload, as text.
fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        format!("panicked: {s}")
    } else if let Some(s) = panic.downcast_ref::<String>() {
        format!("panicked: {s}")
    } else {
        "panicked".to_owned()
    }
}

#[cfg(test)]
mod tests;

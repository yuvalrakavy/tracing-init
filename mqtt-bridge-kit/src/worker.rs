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
use tracing::{debug, info, warn, Instrument, Span};

use crate::driver::{DeviceDriver, DriverInfo, Feedback, LinkError, OpError, PropertyMap, Scope, TargetState, Writes};
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
    current: Option<Episode>,
}

struct Episode {
    since: Instant,
    attempts: u32,
    warned: bool,
}

impl DeviceOutage {
    fn new(warn_after: Duration) -> DeviceOutage {
        DeviceOutage { warn_after, current: None }
    }

    /// The link was lost, or an attempt to connect (or prove) failed.
    fn failed(&mut self, error: &LinkError) {
        let Some(episode) = &mut self.current else {
            self.current = Some(Episode { since: Instant::now(), attempts: 1, warned: false });
            info!(kind = "device_connection_lost", error = %error, "Device unreachable; retrying");
            return;
        };
        episode.attempts += 1;
        let down_for_ms = episode.since.elapsed().as_millis() as u64;
        if !episode.warned && episode.since.elapsed() >= self.warn_after {
            episode.warned = true;
            warn!(kind = "device_unreachable", error = %error, attempts = episode.attempts, down_for_ms, "Device unreachable; still retrying");
        } else {
            debug!(error = %error, attempts = episode.attempts, down_for_ms, "Device still unreachable");
        }
    }

    /// Proven alive, with the state held meanwhile applied.
    fn recovered(&mut self, applied: usize, refused: u32, expired: u32) {
        match self.current.take() {
            Some(episode) => info!(
                kind = "device_recovered",
                down_for_ms = episode.since.elapsed().as_millis() as u64,
                attempts = episode.attempts,
                applied,
                refused,
                expired,
                "Device reachable again"
            ),
            None => info!(applied, "Connected to the device"),
        }
    }
}

/// One driver operation, under `bound` and the link's `link_lost`: an overrun or a lost link is a
/// `LinkError`.
async fn op<T>(
    handle: &LinkHandle,
    bound: Duration,
    what: &str,
    operation: impl Future<Output = Result<T, OpError>>,
) -> Result<T, OpError> {
    let deadline = Instant::now() + bound;
    handle.set_deadline(Some(deadline));
    // WAIT: device-op
    let result = tokio::select! {
        done = tokio::time::timeout_at(deadline.into(), operation) => match done {
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
/// before the outage (`None`: none, a first install).
pub(crate) fn resolve(value: &StateValue, current: Option<&Value>, before: Option<&Value>) -> Resolution {
    let Some(current) = current else {
        return Resolution::Apply;
    };
    if *current == value.value {
        return Resolution::Satisfied;
    }
    match before {
        Some(before) if before != current => match value.conflict {
            Conflict::DeviceWins => Resolution::Superseded,
            Conflict::RequestWins => Resolution::Apply,
        },
        _ => Resolution::Apply,
    }
}

/// Values by target and property, for the conflict rule.
type Values = HashMap<(String, String), Value>;

fn index(states: &[TargetState]) -> Values {
    let mut values = HashMap::new();
    for state in states {
        for (property, value) in &state.values {
            values.insert((state.target.clone(), property.clone()), value.clone());
        }
    }
    values
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
    /// What the bridge knew of the device when its link was last lost from `Up`: the conflict
    /// rule's "before" values for a device outage. Before any, the read-back stands for it.
    before: Option<BTreeMap<String, PropertyMap>>,
    up: bool,
}

impl<D: DeviceDriver> Worker<D> {
    pub(crate) fn new(driver: D, info: DriverInfo, instance: &str, mailbox: Arc<Mailbox>, reports: Arc<Reports>) -> Worker<D> {
        Worker {
            driver,
            outage: DeviceOutage::new(info.timing.outage_warn),
            info,
            instance: instance.to_owned(),
            mailbox,
            reports,
            abandoned: Arc::new(Abandoned::default()),
            clock: Arc::new(OpClock::new()),
            generation: 0,
            last_attempt: None,
            before: None,
            up: false,
        }
    }

    /// The clock the watchdog on the MQTT side reads.
    pub(crate) fn clock(&self) -> Arc<OpClock> {
        self.clock.clone()
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
        self.up = true;
        self.reports.status(LinkStatus::Connected);
        let applied = match self.catch_up(handle, read).await {
            Ok(applied) => applied,
            Err(e) => return e,
        };
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
                self.mailbox.proven();
                self.fold(&states);
                Ok(Some(states))
            }
            Err(OpError::Link(e)) => Err(e),
            // WAIT: device-op
            Err(OpError::Unsupported(_)) => match op(handle, timing.probe_deadline, "probe", self.driver.probe()).await {
                Err(OpError::Link(e)) => Err(e),
                _ => Ok(None),
            },
            Err(other) => {
                debug!(error = %other, "The device answered the proving read with something unusable");
                Ok(None)
            }
        }
    }

    /// The conflict rule's "before" values: the last device outage's, over the read-back.
    fn before(&self) -> Values {
        let mut before = HashMap::new();
        index_map(&self.reports.read_back_all(), &mut before);
        if let Some(snapshot) = &self.before {
            index_map(snapshot, &mut before);
        }
        before
    }

    /// Entering `Up`: what was held while the device was down, decided by the conflict rule
    /// against the proving read, then applied a target at a time; then, for an unacknowledged
    /// device, a probe proves it taken. Returns how many values were written.
    async fn catch_up(&mut self, handle: &LinkHandle, read: Option<Vec<TargetState>>) -> Result<usize, LinkError> {
        let current = read.as_deref().map(index).unwrap_or_default();
        let before = self.before();
        let mut applied = 0;
        loop {
            let held = self.mailbox.take_held(|v| {
                let key = (v.target.clone(), v.property.clone());
                resolve(v, current.get(&key), before.get(&key))
            });
            match held {
                None => break,
                Some(Held::Config { target, config }) => self.configure(handle, &target, config).await?,
                Some(Held::Values(held)) => {
                    self.superseded(&held.superseded);
                    if held.satisfied > 0 {
                        debug!(target = %held.target, satisfied = held.satisfied, "Held requests the device already has");
                    }
                    applied += self.write_values(handle, &held.target, held.apply).await?;
                }
            }
        }
        if self.info.writes == Writes::Unacknowledged && self.mailbox.unproven_since().is_some() {
            self.probe(handle).await?;
        }
        Ok(applied)
    }

    /// Requests a change at the device won: dropped, each reported on `Error`.
    fn superseded(&self, values: &[StateValue]) {
        for value in values {
            info!(target = %value.target, property = %value.property, "A held request was superseded by a change at the device");
            self.reports.error(
                ErrorReport::new(Reason::Superseded, "the device was changed during the outage; the change at the device wins")
                    .target(Some(&value.target))
                    .property(&value.property),
            );
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
                if feedback == Feedback::Assumed && self.reports.reports_for(target) == reported_before {
                    self.reports.state(target, map, true);
                }
                Ok(values.len())
            }
            Err(OpError::Link(e)) => Err(e),
            Err(failure) => {
                self.mailbox.failed();
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
                    self.reports.error(ErrorReport::new(reason, message).target(Some(target)).property(&value.property));
                }
                Ok(0)
            }
        }
    }

    /// Hand a target its setup.
    async fn configure(&mut self, handle: &LinkHandle, target: &str, config: Option<Value>) -> Result<(), LinkError> {
        let timing = self.info.timing;
        // WAIT: device-op
        match op(handle, timing.operation, "configure", self.driver.configure(target, config.as_ref())).await {
            Ok(()) => {
                self.mailbox.applied(false);
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
        match op(handle, timing.probe_deadline, "probe", self.driver.probe()).await {
            Err(OpError::Link(e)) => Err(e),
            // An answer, even an unusable one, was written after every unproven write; a driver
            // that cannot probe leaves nothing better to wait for.
            _ => {
                self.mailbox.proven();
                Ok(())
            }
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

    /// `Up`: the mailbox, polls and probes, until the link is lost.
    async fn serve(&mut self, handle: &LinkHandle) -> LinkError {
        let timing = self.info.timing;
        let mut last_read = Instant::now();
        let mut last_heard = Instant::now();
        let mailbox = self.mailbox.clone();
        loop {
            let poll_at = timing.poll.map(|p| last_read + p);
            let probe_at = self.probe_due(last_heard);
            // WAIT: worker-up
            let step = tokio::select! {
                biased;
                lost = handle.lost() => return lost,
                () = sleep_until(probe_at) => Step::Probe,
                () = sleep_until(poll_at) => Step::Poll,
                work = mailbox.take() => Step::Work(work),
            };
            let outcome = match step {
                Step::Probe => self.probe(handle).await.map(|()| last_heard = Instant::now()),
                Step::Poll => {
                    last_read = Instant::now();
                    // WAIT: device-op
                    match op(handle, timing.operation, "read_state", self.driver.read_state(Scope::All)).await {
                        Ok(states) => {
                            last_heard = Instant::now();
                            self.mailbox.proven();
                            self.fold(&states);
                            Ok(())
                        }
                        Err(OpError::Link(e)) => Err(e),
                        Err(other) => {
                            debug!(error = %other, "A poll's answer was unusable");
                            Ok(())
                        }
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
            Work::Apply { target, values, check } => {
                let values = if check { self.check(handle, &target, values).await? } else { values };
                self.write_values(handle, &target, values).await.map(drop)
            }
            Work::Configure { target, config } => self.configure(handle, &target, config).await,
            Work::Execute { target, call, reply } => {
                let result = op(handle, timing.operation, "execute", self.driver.execute(target.as_deref().unwrap_or(""), &call)).await;
                self.answered(target.as_deref(), &call.command, result, reply.into_iter().collect(), true)
            }
            Work::Read { key: ReadKey::Refresh(target), replies, .. } => {
                let scope = target.clone().map_or(Scope::All, Scope::Target);
                // WAIT: device-op
                let result = op(handle, timing.operation, "read_state", self.driver.read_state(scope)).await.map(|states| {
                    self.mailbox.proven();
                    self.fold(&states);
                    None
                });
                self.answered(target.as_deref(), "refresh", result, replies, false)
            }
            Work::Read { key: ReadKey::Command { target, command }, call, replies } => {
                let call = call.unwrap_or_else(|| crate::driver::CommandCall { command: command.clone(), params: PropertyMap::new() });
                let result = op(handle, timing.operation, "execute", self.driver.execute(target.as_deref().unwrap_or(""), &call)).await;
                self.answered(target.as_deref(), &command, result, replies, true)
            }
        }
    }

    /// A checked value taken while the device is up (the broker replayed it as retained): read the
    /// target first, and let the conflict rule decide. Returns what to apply.
    async fn check(&mut self, handle: &LinkHandle, target: &str, values: Vec<StateValue>) -> Result<Vec<StateValue>, LinkError> {
        let timing = self.info.timing;
        // WAIT: device-op
        let current = match op(handle, timing.operation, "read_state", self.driver.read_state(Scope::Target(target.to_owned()))).await {
            Ok(states) => {
                self.mailbox.proven();
                self.fold(&states);
                index(&states)
            }
            Err(OpError::Link(e)) => return Err(e),
            Err(_) => HashMap::new(),
        };
        let before = self.before();
        let mut apply = Vec::new();
        let mut superseded = Vec::new();
        for value in values {
            let key = (value.target.clone(), value.property.clone());
            match resolve(&value, current.get(&key), before.get(&key)) {
                Resolution::Apply => apply.push(value),
                Resolution::Satisfied => {}
                Resolution::Superseded => superseded.push(value),
            }
        }
        self.mailbox.retain_in_flight(|v| apply.iter().any(|a| a.target == v.target && a.property == v.property));
        self.superseded(&superseded);
        Ok(apply)
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
        if std::mem::take(&mut self.up) {
            self.before = Some(self.reports.snapshot());
        }
        // Watched like an operation: a disconnect that blocks the thread is a dead worker too.
        self.clock.set(Some(Instant::now() + self.info.timing.operation));
        // WAIT: device-disconnect
        let _ = tokio::time::timeout(self.info.timing.operation, self.driver.disconnect()).await;
        self.clock.set(None);
        let unanswered = self.mailbox.disconnected();
        report_refused(&self.reports, unanswered);
        self.reports.status(LinkStatus::Unreachable);
        self.outage.failed(&error);
    }
}

/// What a disconnect refused: each momentary command on `Error`, and every reply waited for.
pub(crate) fn report_refused(reports: &Reports, unanswered: Vec<Unanswered>) {
    let message = "the device link was lost before it ran";
    for u in unanswered {
        if u.momentary {
            reports.error(ErrorReport::new(Reason::Refused, message).target(u.target.as_deref()).command(&u.command));
        }
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
    std::thread::Builder::new().name("bridge-worker".into()).spawn(move || {
        let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("its runtime: {e}"))?;
            let mut worker = worker;
            // WAIT: worker-thread
            runtime.block_on(
                async {
                    // WAIT: worker-stop
                    tokio::select! {
                        () = worker.run() => {}
                        _ = stop.wait_for(|stop| *stop) => {}
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

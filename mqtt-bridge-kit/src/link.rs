//! The link context (Bridge Runtime spec §4.2): what a driver is given to report, run helpers, say
//! the link died, and make a synchronous call under the current deadline.
//!
//! A [`LinkContext`] lives as long as one device link: the worker makes it for `connect` and drops
//! it when the link is lost, which cancels every helper started through it. A driver that needs the
//! context's calls in its later operations keeps a [`LinkHandle`] (`ctx.handle()`), which a stale
//! link's helpers may still hold harmlessly: a lost link reported through a handle of an earlier
//! link is ignored.
//!
//! Every reporter call returns at once and never waits on MQTT: state coalesces per target in the
//! retained model, events go to a capped queue whose overflow is counted and logged once per
//! episode ([`crate::reports`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use lock_order::sync::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{error, Instrument};

use crate::driver::{LinkError, PropertyMap};
use crate::reports::{Outbound, Reports};

tokio::task_local! {
    /// The deadline of the driver operation running in this task: set by the worker around each
    /// operation ([`under_deadline`]), so a blocking call made from the operation is held to it,
    /// and one made from a helper — a task of its own — has none (spec §4.2; the R1 re-gate's R14:
    /// the deadline was the link's, and a helper's call that overlapped an operation inherited it).
    static OP_DEADLINE: Instant;
}

/// Run `operation` with `deadline` as its blocking calls' deadline.
pub(crate) async fn under_deadline<F: Future>(deadline: Instant, operation: F) -> F::Output {
    // WAIT: device-op
    OP_DEADLINE.scope(deadline, operation).await
}

/// Operations' blocking calls whose threads may run at once — in use, or abandoned (past their
/// deadline, or their caller cancelled) and still running; past it, `blocking` refuses with a lost
/// link until one of them ends, and a cap that stays full is a dead worker.
pub const ABANDONED_CAP: usize = 4;

/// Helpers' blocking calls whose threads may run at once: a budget of their own, so long-lived
/// reads (a reader per port or zone) never fill the operations' cap (re-gate 3 S7). Past it a
/// helper's call is refused with a lost link; it does not end the process.
pub const HELPER_CAP: usize = 16;

/// Whose a blocking call is: an operation's (its task has a deadline) or a helper's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Operation,
    Helper,
}

/// Reports from a driver or its helpers, handed over without waiting (`ctx.report`, or
/// [`LinkHandle::report`]). Cheap to clone and `Send`, so a helper or a blocking thread can hold
/// one. Nothing here ever waits on MQTT: `State` coalesces per target into the retained model;
/// events go to a capped queue whose overflow is counted and logged once per episode.
#[derive(Debug, Clone)]
pub struct Reporter {
    reports: Arc<Reports>,
}

/// The bridge's start-up read-back of its own retained `State`, as the conflict rule reads it:
/// frozen when it was first done. Its values are what the previous run last published — its
/// reports and assumptions as retained by the broker — not evidence from the device.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReadBack {
    /// Whether the read-back is done: `false` while its SUBACK is pending, and for good when the
    /// broker refused it or it came late (then `targets` holds what arrived before that).
    pub complete: bool,
    /// Each target's retained `State` values.
    pub targets: std::collections::BTreeMap<String, PropertyMap>,
}

impl Reporter {
    /// Check the original dequeued request immediately before an actuator commit.
    /// Exact model replacement retires it even when visible generations match.
    /// Keep this context unchanged through any driver-owned preparation queue.
    pub fn request_is_current(&self, target: Option<&str>, context: &crate::RequestContext) -> bool {
        self.reports.context_is_current(target, context)
    }
    /// Capture the exact active momentary command, only while its execute future
    /// is being polled. Helpers, validation and reads receive none. Forward this
    /// explicitly to an inventory owner; it is not an MQTT generation token.
    pub fn command_completion(&self) -> Option<crate::CommandCompletion> {
        crate::completion::current()
    }
    /// Install a durable inventory while preserving only the exact active
    /// command that committed it. The capability is consumed on successful CAS;
    /// a stale, foreign, used or cut-off command fails before authority changes.
    /// All other retired work is refused normally; final reply metadata stays
    /// original. Ordinary administrative edits use `replace_generations`.
    pub fn replace_generations_for_command(
        &self,
        expected: &crate::TargetGenerations,
        next: crate::TargetGenerations,
        completion: &crate::CommandCompletion,
    ) -> Result<(), String> {
        self.reports.replace_generations_for_command(expected, next, completion)
    }
    /// Install an already durable inventory mapping and retire old runtime work
    /// before publishing the inventory or exposing the new mapping to commands.
    pub fn replace_generations(&self, expected: &crate::TargetGenerations, next: crate::TargetGenerations) -> Result<(), String> {
        self.reports.replace_generations(expected, next)
    }
    /// The start-up read-back (see [`ReadBack`]). Never waits.
    pub(crate) fn read_back(&self) -> ReadBack {
        let (complete, targets) = self.reports.read_back_view();
        ReadBack { complete, targets }
    }

    pub(crate) fn new(reports: Arc<Reports>) -> Reporter {
        Reporter { reports }
    }

    /// A target's observed state — or, with `assumed`, what the bridge believes it is without
    /// having read it back. Merged into the target's retained `State`. Never waits. Each name must
    /// stand as one topic segment (no `/`, `+`, `#`, not empty): one that cannot is dropped, a
    /// driver bug, ERROR `driver_contract_violation`.
    pub fn state(&self, target: &str, values: PropertyMap, assumed: bool) {
        self.reports.state(target, values, assumed);
    }

    /// Report values read under this original mapping generation.
    pub fn state_with_context(&self, target: &str, values: PropertyMap, assumed: bool, context: &crate::RequestContext) {
        self.reports.state_with_context(target, values, assumed, context);
    }

    /// Retract a target gone from the device (a unit no longer in the controller's list): its
    /// retained `State` is cleared — an empty retained payload — and it is left out of the model,
    /// until it is reported again. Never waits.
    pub fn remove(&self, target: &str) {
        self.reports.remove(target);
    }

    /// Retract only this current mapping's state.
    pub fn remove_with_context(&self, target: &str, context: &crate::RequestContext) {
        self.reports.remove_with_context(target, context);
    }

    /// Retract one property of a target: left out of its `State` from now on, until reported
    /// again. Never waits.
    pub fn remove_property(&self, target: &str, property: &str) {
        self.reports.remove_property(target, property);
    }

    /// Remove a property only while this mapping remains current.
    pub fn remove_property_with_context(&self, target: &str, property: &str, context: &crate::RequestContext) {
        self.reports.remove_property_with_context(target, property, context);
    }

    /// Something that happened, for the `Event` lane: a JSON object with a string `kind` (and a
    /// `target` where it concerns one). Never waits; anything else is a driver bug, logged as an
    /// ERROR (`driver_contract_violation`) and dropped.
    pub fn event(&self, event: Value) {
        self.event_with_context(event, &crate::RequestContext::default());
    }

    /// An unsolicited event retains its original target or instance generation.
    pub fn event_with_context(&self, event: Value, context: &crate::RequestContext) {
        let ok = event.get("kind").is_some_and(Value::is_string);
        if !ok {
            error!(kind = "driver_contract_violation", event = %event, "An event without a string `kind` was dropped");
            return;
        }
        let target = event.get("target").and_then(Value::as_str);
        if event.get("target").is_some() && !target.is_some_and(crate::topics::valid_segment) {
            error!(kind = "driver_contract_violation", "An event with an invalid target was dropped");
            return;
        }
        self.reports.observed_event(
            target,
            Outbound { context: context.clone(), topic: None, payload: event.to_string().into_bytes(), correlation: None },
        );
    }
}

/// The blocking calls whose threads are running, across links: in use, or abandoned.
#[derive(Debug)]
pub(crate) struct Abandoned {
    running: AtomicUsize,
    /// Helpers' calls running, under [`HELPER_CAP`].
    helpers: AtomicUsize,
    epoch: Instant,
    /// Since when an operation's call was refused for want of a place, in milliseconds since
    /// `epoch` plus one; 0 once a place is free again. A cap that stays full is a dead worker
    /// (re-gate R13).
    full_since: AtomicU64,
    /// The same, for the helpers' budget: one full that long is a dead worker too — every link is
    /// lost at its reader's first call, for good (re-gate 4 F4).
    helpers_full_since: AtomicU64,
}

impl Default for Abandoned {
    fn default() -> Abandoned {
        Abandoned {
            running: AtomicUsize::new(0),
            helpers: AtomicUsize::new(0),
            epoch: Instant::now(),
            full_since: AtomicU64::new(0),
            helpers_full_since: AtomicU64::new(0),
        }
    }
}

impl Abandoned {
    /// How long calls of a kind have been refused for want of a place, with none freed since —
    /// the longer of the two budgets' — and whose budget it is.
    pub(crate) fn full_for(&self) -> Option<(Duration, &'static str)> {
        let since = |stamp: &AtomicU64| match stamp.load(Ordering::SeqCst) {
            0 => None,
            stamp => Some(self.epoch.elapsed().saturating_sub(Duration::from_millis(stamp - 1))),
        };
        let operations = since(&self.full_since).map(|full| (full, "operations'"));
        let helpers = since(&self.helpers_full_since).map(|full| (full, "helpers'"));
        operations.into_iter().chain(helpers).max_by_key(|(full, _)| *full)
    }

    /// A kind's count, cap and stamp.
    fn budget(&self, kind: Kind) -> (&AtomicUsize, usize, &AtomicU64) {
        match kind {
            Kind::Operation => (&self.running, ABANDONED_CAP, &self.full_since),
            Kind::Helper => (&self.helpers, HELPER_CAP, &self.helpers_full_since),
        }
    }

    /// A call of `kind` was refused for want of a place: stamped, unless a place was freed since —
    /// a release just before the stamp would leave it standing with places free (re-gate 4 minor 3).
    fn refused(&self, kind: Kind) {
        let (count, cap, full_since) = self.budget(kind);
        let stamp = self.epoch.elapsed().as_millis() as u64 + 1;
        if full_since.compare_exchange(0, stamp, Ordering::SeqCst, Ordering::SeqCst).is_ok() && count.load(Ordering::SeqCst) < cap {
            let _ = full_since.compare_exchange(stamp, 0, Ordering::SeqCst, Ordering::SeqCst);
        }
    }
}

/// One blocking call's place under [`ABANDONED_CAP`]: reserved before its thread starts, and held
/// by the thread until it ends — whatever becomes of the future that started it, so a call whose
/// caller was cancelled stays counted while its thread runs (review C3). Released on the thread's
/// end, its panic included, and at once if the thread never starts.
struct Slot(Arc<Abandoned>, Kind);

impl Slot {
    fn reserve(abandoned: &Arc<Abandoned>, kind: Kind) -> Option<Slot> {
        let (count, cap, full_since) = abandoned.budget(kind);
        // A compare-exchange loop, not `fetch_update` (deprecated from Rust 1.99, its successor
        // `try_update` absent before it): the same atomic reservation on every toolchain.
        let mut running = count.load(Ordering::SeqCst);
        let slot = loop {
            if running >= cap {
                break None;
            }
            match count.compare_exchange_weak(running, running + 1, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(_) => break Some(Slot(abandoned.clone(), kind)),
                Err(now) => running = now,
            }
        };
        match slot {
            // A place had: the budget is not full, whatever a refusal that lost the race with a
            // release stamped (re-gate 3 S6).
            Some(_) => full_since.store(0, Ordering::SeqCst),
            None => abandoned.refused(kind),
        }
        slot
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        let (count, _, full_since) = self.0.budget(self.1);
        count.fetch_sub(1, Ordering::SeqCst);
        full_since.store(0, Ordering::SeqCst);
    }
}

/// The deadline of the operation the device worker is running, and the worker runtime's
/// heartbeat, for the watchdog on the MQTT side ([`crate::process`]): a worker whose thread is
/// blocked in a synchronous call cannot fire its own timers, so only another thread can see the
/// deadline pass, or the heartbeat stop. Across links.
#[derive(Debug)]
pub(crate) struct OpClock {
    epoch: Instant,
    /// In milliseconds since `epoch` plus one; 0 between operations.
    deadline: AtomicU64,
    /// The worker runtime's last heartbeat, in milliseconds since `epoch` plus one; 0 before the
    /// first.
    beat: AtomicU64,
}

impl OpClock {
    pub(crate) fn new() -> OpClock {
        OpClock { epoch: Instant::now(), deadline: AtomicU64::new(0), beat: AtomicU64::new(0) }
    }

    /// The worker's runtime is running: a ticker on it stamps this (re-gate R1).
    pub(crate) fn beat(&self) {
        self.beat.store(self.epoch.elapsed().as_millis() as u64 + 1, Ordering::SeqCst);
    }

    /// How long since the last heartbeat; `None` before the first.
    pub(crate) fn since_beat(&self) -> Option<Duration> {
        match self.beat.load(Ordering::SeqCst) {
            0 => None,
            stamp => Some(self.epoch.elapsed().saturating_sub(Duration::from_millis(stamp - 1))),
        }
    }

    pub(crate) fn set(&self, deadline: Option<Instant>) {
        let stamp = deadline.map_or(0, |d| d.saturating_duration_since(self.epoch).as_millis() as u64 + 1);
        self.deadline.store(stamp, Ordering::SeqCst);
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        match self.deadline.load(Ordering::SeqCst) {
            0 => None,
            stamp => Some(self.epoch + Duration::from_millis(stamp - 1)),
        }
    }
}

/// One link's shared state.
#[derive(Debug)]
struct LinkShared {
    generation: u64,
    lost: AtomicBool,
    reason: OnceLock<String>,
    lost_notify: Notify,
    abandoned: Arc<Abandoned>,
    /// The worker's operation clock, which the watchdog reads.
    clock: Arc<OpClock>,
    instance: String,
    /// The link's helpers, to abort when the link is dropped; `None` once it is.
    helpers: Mutex<Option<Vec<tokio::task::AbortHandle>>>,
    /// The device worker's thread: helpers run on its runtime, so only it may start them.
    thread: std::thread::ThreadId,
}

/// Start a helper for `handle`'s link (see [`LinkContext::spawn`]); `critical`, its return is the
/// link lost (see [`LinkContext::spawn_transient`] for one that may end).
fn spawn_helper<F>(handle: &LinkHandle, task: F, critical: bool)
where
    F: Future<Output = ()> + 'static,
{
    if std::thread::current().id() != handle.shared.thread {
        error!(kind = "driver_contract_violation", "A helper was started off the device worker's thread; it is not started");
        return;
    }
    let watched = Watched { task: Box::pin(task), handle: handle.clone(), critical }.in_current_span();
    // WAIT: link-helpers
    let mut helpers = handle.shared.helpers.lock().unwrap_or_else(|p| p.into_inner());
    // A link already dropped starts no helper.
    if let Some(helpers) = helpers.as_mut() {
        helpers.push(tokio::task::spawn_local(watched).abort_handle());
    }
}

/// A handle on one link, for a driver's later operations and its helpers. Cheap to clone.
#[derive(Debug, Clone)]
pub struct LinkHandle {
    shared: Arc<LinkShared>,
    report: Reporter,
}

impl LinkHandle {
    /// The reporter.
    pub fn report(&self) -> &Reporter {
        &self.report
    }

    /// The bridge's start-up read-back of its retained `State` — the previous run's reports, not
    /// device evidence; `complete` says whether it is done (see [`ReadBack`]). Never waits.
    pub fn read_back(&self) -> ReadBack {
        self.report.read_back()
    }

    /// The link died, as a helper noticed (an unsolicited-input reader that reads end of file).
    /// The first reason is kept; one through a handle of an earlier link is ignored.
    pub fn link_lost(&self, reason: impl Into<String>) {
        let _ = self.shared.reason.set(reason.into());
        if !self.shared.lost.swap(true, Ordering::SeqCst) {
            self.shared.lost_notify.notify_waiters();
            self.shared.lost_notify.notify_one();
        }
    }

    /// Whether the link was reported lost.
    pub fn is_lost(&self) -> bool {
        self.shared.lost.load(Ordering::SeqCst)
    }

    /// The link's number: each connect gets the next.
    pub fn generation(&self) -> u64 {
        self.shared.generation
    }

    /// The bridge's instance.
    pub fn instance(&self) -> &str {
        &self.shared.instance
    }

    /// Start a helper for this link, as [`LinkContext::spawn`] does — from a driver's later
    /// operation or another helper. Only on the device worker's thread, where helpers run: called
    /// elsewhere it starts nothing (ERROR `driver_contract_violation`); on a link already lost, it
    /// starts nothing either.
    pub fn spawn<F>(&self, task: F)
    where
        F: Future<Output = ()> + 'static,
    {
        spawn_helper(self, task, true);
    }

    /// Start a transient helper for this link, as [`LinkContext::spawn_transient`] does.
    pub fn spawn_transient<F>(&self, task: F)
    where
        F: Future<Output = ()> + 'static,
    {
        spawn_helper(self, task, false);
    }

    /// The link's loss, if it was reported lost.
    pub(crate) fn loss(&self) -> Option<LinkError> {
        self.is_lost().then(|| LinkError::new(self.shared.reason.get().cloned().unwrap_or_else(|| "the link was reported lost".into())))
    }

    /// Wait until the link is reported lost; its reason. Cancellation-safe.
    pub(crate) async fn lost(&self) -> LinkError {
        loop {
            // WAIT: link-lost
            let notified = self.shared.lost_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(lost) = self.loss() {
                return lost;
            }
            // WAIT: link-lost
            notified.await;
        }
    }

    /// Set the deadline of the operation about to run (`None` between operations), for the
    /// watchdog. Its blocking calls take it from their task ([`under_deadline`]).
    pub(crate) fn set_deadline(&self, deadline: Option<Instant>) {
        self.shared.clock.set(deadline);
    }

    /// Run a synchronous library call on a thread of its own — called from a driver operation,
    /// under that operation's deadline (an operation's task carries it); from a helper, a task of
    /// its own, with none, even while an operation runs. A call that overruns its deadline is
    /// abandoned — a thread cannot be cancelled — the link is reported lost, and the error is a
    /// `LinkError`. A call that panics is a `LinkError` too, without a loss.
    ///
    /// Every call's thread counts, from before it starts until it ends — in use or abandoned, a
    /// helper's (no deadline) too, and one whose caller was cancelled — against a budget by kind:
    /// [`ABANDONED_CAP`] for operations' calls, [`HELPER_CAP`] for helpers'. Past its budget a call
    /// is refused with a `LinkError` at once, and the link reported lost. A budget that keeps
    /// refusing, with no place freed, for twice the operation bound is a dead worker (the process
    /// exits for its supervisor to restart): its threads are stuck for good. So give a synchronous
    /// call its own timeout where the library allows, and do not park a long-lived read per
    /// reconnect.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, LinkError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let kind = if OP_DEADLINE.try_with(|_| ()).is_ok() { Kind::Operation } else { Kind::Helper };
        let Some(slot) = Slot::reserve(&self.shared.abandoned, kind) else {
            self.link_lost("too many blocking calls are still running");
            let cap = if kind == Kind::Operation { ABANDONED_CAP } else { HELPER_CAP };
            return Err(LinkError::new(format!("{cap} blocking calls of the kind are still running")));
        };
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let spawned = std::thread::Builder::new().name("bridge-blocking".into()).spawn(move || {
            let _slot = slot;
            let value = f();
            let _ = tx.send(value);
        });
        if let Err(e) = spawned {
            // The call cannot run: the link is lost, as for any blocking call refused (re-gate 4
            // minor 5).
            self.link_lost("a blocking call's thread could not start");
            return Err(LinkError::new(format!("a blocking call's thread could not start: {e}")));
        }
        let outcome = match OP_DEADLINE.try_with(|deadline| *deadline).ok() {
            // WAIT: blocking-call
            Some(deadline) => tokio::time::timeout_at(deadline.into(), &mut rx).await,
            // WAIT: blocking-call
            None => Ok((&mut rx).await),
        };
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(LinkError::new("a blocking call panicked")),
            Err(_) => {
                // It may have finished as the deadline passed.
                if let Ok(value) = rx.try_recv() {
                    return Ok(value);
                }
                self.link_lost("a blocking call overran its deadline");
                Err(LinkError::new("a blocking call overran its deadline and was abandoned"))
            }
        }
    }
}

/// A helper, watched: its end is the link's loss. A helper the link cannot run without that returns
/// (it read end of file, or gave up) has lost the link without saying so; one that panics is a
/// driver bug too. Unwatched, a listening driver stayed "connected" and deaf (review M5).
struct Watched<F> {
    task: Pin<Box<F>>,
    handle: LinkHandle,
    /// The link cannot run without it: its return is the link lost.
    critical: bool,
}

impl<F: Future<Output = ()>> Future for Watched<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.task.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => {
                if this.critical {
                    this.handle.link_lost("a helper of the link ended");
                }
                Poll::Ready(())
            }
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                error!(kind = "driver_contract_violation", error = %message, "A helper of the device link panicked; the link is lost");
                this.handle.link_lost("a helper of the link panicked");
                Poll::Ready(())
            }
        }
    }
}

/// What a driver's `connect` is given (spec §4.2): the reporter, the helper spawners, a way to say
/// the link died, and a bounded synchronous call. One lives as long as one device link: the worker
/// makes it for `connect` and drops it when the link is lost, which cancels every helper started
/// through it. To use its calls from later operations, keep [`LinkContext::handle`].
pub struct LinkContext {
    /// The reporter: observed or assumed state, and events. A clone may be kept.
    pub report: Reporter,
    handle: LinkHandle,
}

/// The link is lost: its helpers are cancelled, and none starts for it from now on.
impl Drop for LinkContext {
    fn drop(&mut self) {
        let helpers = {
            // WAIT: link-helpers
            let mut helpers = self.handle.shared.helpers.lock().unwrap_or_else(|p| p.into_inner());
            helpers.take()
        };
        for helper in helpers.into_iter().flatten() {
            helper.abort();
        }
    }
}

impl LinkContext {
    pub(crate) fn new(
        generation: u64,
        instance: &str,
        reports: Arc<Reports>,
        abandoned: Arc<Abandoned>,
        clock: Arc<OpClock>,
    ) -> LinkContext {
        let report = Reporter::new(reports);
        let shared = Arc::new(LinkShared {
            generation,
            lost: AtomicBool::new(false),
            reason: OnceLock::new(),
            lost_notify: Notify::new(),
            abandoned,
            clock,
            instance: instance.to_owned(),
            helpers: Mutex::new("link-helpers", Some(Vec::new())),
            thread: std::thread::current().id(),
        });
        LinkContext { report: report.clone(), handle: LinkHandle { shared, report } }
    }

    /// Start a helper the runtime owns — an unsolicited-input reader, say — for the life of the
    /// link: cancelled when the link drops, and watched — a helper that ends, returning or
    /// panicking, is the link lost (a panic is also a driver bug, ERROR `driver_contract_violation`).
    /// It runs on the device worker's own single-threaded runtime, as the driver does, so it need
    /// not be `Send`.
    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = ()> + 'static,
    {
        spawn_helper(&self.handle, task, true);
    }

    /// Start a transient helper — a delayed re-query, a debounce timer — for the life of the link at
    /// most: cancelled when the link drops, like any helper, but free to end; its return is not a
    /// lost link. A panic still is: a driver bug, ERROR `driver_contract_violation` (re-gate R17).
    pub fn spawn_transient<F>(&mut self, task: F)
    where
        F: Future<Output = ()> + 'static,
    {
        spawn_helper(&self.handle, task, false);
    }

    /// The link died, as the driver noticed outside an operation.
    pub fn link_lost(&self, reason: impl Into<String>) {
        self.handle.link_lost(reason);
    }

    /// A synchronous call under the current deadline: see [`LinkHandle::blocking`].
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, LinkError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.handle.blocking(f).await
    }

    /// A handle on this link, to keep.
    pub fn handle(&self) -> LinkHandle {
        self.handle.clone()
    }

    /// The bridge's start-up read-back of its retained `State`, as at `connect`: the previous run's
    /// reports, not device evidence. At the first connect it may not be done yet — `complete` says
    /// so; keep the [`handle`](Self::handle) to read it again later (see [`ReadBack`]). Never waits.
    pub fn read_back(&self) -> ReadBack {
        self.handle.read_back()
    }

    /// The bridge's instance.
    pub fn instance(&self) -> &str {
        self.handle.instance()
    }
}

#[cfg(test)]
mod tests;

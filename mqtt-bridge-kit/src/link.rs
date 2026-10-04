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
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::Notify;
use tracing::{error, Instrument};

use crate::driver::{LinkError, PropertyMap};
use crate::reports::{Outbound, Reports};

/// Blocking calls past their deadline that may still be running; past it, `blocking` refuses with
/// a lost link until one of them ends.
pub const ABANDONED_CAP: usize = 4;

const RUNNING: u8 = 0;
const FINISHED: u8 = 1;
const ABANDONED: u8 = 2;

/// Reports from a driver or its helpers, handed over without waiting.
#[derive(Debug, Clone)]
pub struct Reporter {
    reports: Arc<Reports>,
}

impl Reporter {
    pub(crate) fn new(reports: Arc<Reports>) -> Reporter {
        Reporter { reports }
    }

    /// A target's observed state — or, with `assumed`, what the bridge believes it is without
    /// having read it back. Merged into the target's retained `State`. Never waits.
    pub fn state(&self, target: &str, values: PropertyMap, assumed: bool) {
        self.reports.state(target, values, assumed);
    }

    /// Something that happened, for the `Event` lane: a JSON object with a string `kind` (and a
    /// `target` where it concerns one). Never waits; anything else is a driver bug, logged as an
    /// ERROR (`driver_contract_violation`) and dropped.
    pub fn event(&self, event: Value) {
        let ok = event.get("kind").is_some_and(Value::is_string);
        if !ok {
            error!(kind = "driver_contract_violation", event = %event, "An event without a string `kind` was dropped");
            return;
        }
        self.reports.event(Outbound { topic: None, payload: event.to_string().into_bytes(), correlation: None });
    }
}

/// The blocking calls abandoned past their deadline and still running, across links.
#[derive(Debug, Default)]
pub(crate) struct Abandoned {
    running: AtomicUsize,
}

/// The deadline of the operation the device worker is running, for the watchdog on the MQTT side
/// ([`crate::process::Running::ended`]): a worker whose thread is blocked in a synchronous call
/// cannot fire its own timers, so only another thread can see the deadline pass. Across links.
#[derive(Debug)]
pub(crate) struct OpClock {
    epoch: Instant,
    /// In milliseconds since `epoch` plus one; 0 between operations.
    deadline: AtomicU64,
}

impl OpClock {
    pub(crate) fn new() -> OpClock {
        OpClock { epoch: Instant::now(), deadline: AtomicU64::new(0) }
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
    /// The current operation's deadline, in milliseconds since `epoch` plus one; 0 when none.
    deadline: AtomicU64,
    epoch: Instant,
    abandoned: Arc<Abandoned>,
    /// The worker's operation clock, which the watchdog reads.
    clock: Arc<OpClock>,
    instance: String,
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

    /// Wait until the link is reported lost; its reason. Cancellation-safe.
    pub(crate) async fn lost(&self) -> LinkError {
        loop {
            // WAIT: link-lost
            let notified = self.shared.lost_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_lost() {
                return LinkError::new(self.shared.reason.get().cloned().unwrap_or_else(|| "the link was reported lost".into()));
            }
            // WAIT: link-lost
            notified.await;
        }
    }

    /// Set the deadline of the operation about to run (`None` between operations), for its
    /// blocking calls and for the watchdog.
    pub(crate) fn set_deadline(&self, deadline: Option<Instant>) {
        let stamp = deadline.map_or(0, |d| d.saturating_duration_since(self.shared.epoch).as_millis() as u64 + 1);
        self.shared.deadline.store(stamp, Ordering::SeqCst);
        self.shared.clock.set(deadline);
    }

    fn deadline(&self) -> Option<Instant> {
        match self.shared.deadline.load(Ordering::SeqCst) {
            0 => None,
            stamp => Some(self.shared.epoch + Duration::from_millis(stamp - 1)),
        }
    }

    /// Run a synchronous library call on a thread of its own, under the current operation's
    /// deadline. A call that overruns it is abandoned — a thread cannot be cancelled — the link is
    /// reported lost, and the error is a `LinkError`. At most [`ABANDONED_CAP`] abandoned calls may
    /// still be running; past it, the call is refused with a `LinkError` at once.
    pub async fn blocking<T, F>(&self, f: F) -> Result<T, LinkError>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        let abandoned = self.shared.abandoned.clone();
        if abandoned.running.load(Ordering::SeqCst) >= ABANDONED_CAP {
            self.link_lost("too many blocking calls overran their deadline and are still running");
            return Err(LinkError::new(format!("{ABANDONED_CAP} blocking calls past their deadline are still running")));
        }
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        // RUNNING, then FINISHED (by the thread) or ABANDONED (by the deadline), whichever is
        // first; an abandoned call is counted until its thread ends.
        let state = Arc::new(AtomicU8::new(RUNNING));
        let thread_state = state.clone();
        let thread_abandoned = abandoned.clone();
        let spawned = std::thread::Builder::new().name("bridge-blocking".into()).spawn(move || {
            let value = f();
            let _ = tx.send(value);
            if thread_state.compare_exchange(RUNNING, FINISHED, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                thread_abandoned.running.fetch_sub(1, Ordering::SeqCst);
            }
        });
        if let Err(e) = spawned {
            return Err(LinkError::new(format!("a blocking call's thread could not start: {e}")));
        }
        let outcome = match self.deadline() {
            // WAIT: blocking-call
            Some(deadline) => tokio::time::timeout_at(deadline.into(), &mut rx).await,
            // WAIT: blocking-call
            None => Ok((&mut rx).await),
        };
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(LinkError::new("a blocking call panicked")),
            Err(_) => {
                // Counted before the race is decided, so the thread's decrement never comes first.
                abandoned.running.fetch_add(1, Ordering::SeqCst);
                if state.compare_exchange(RUNNING, ABANDONED, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                    // It finished as the deadline passed: not abandoned after all.
                    abandoned.running.fetch_sub(1, Ordering::SeqCst);
                    if let Ok(value) = rx.try_recv() {
                        return Ok(value);
                    }
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
}

impl<F: Future<Output = ()>> Future for Watched<F> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = &mut *self;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| this.task.as_mut().poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(())) => {
                this.handle.link_lost("a helper of the link ended");
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

/// What a driver's `connect` is given (spec §4.2).
pub struct LinkContext {
    /// The reporter: observed or assumed state, and events.
    pub report: Reporter,
    handle: LinkHandle,
    /// The link's helpers, aborted when the context is dropped (the link lost).
    helpers: tokio::task::JoinSet<()>,
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
            deadline: AtomicU64::new(0),
            epoch: Instant::now(),
            abandoned,
            clock,
            instance: instance.to_owned(),
        });
        LinkContext { report: report.clone(), handle: LinkHandle { shared, report }, helpers: tokio::task::JoinSet::new() }
    }

    /// Start a helper the runtime owns — an unsolicited-input reader, say — for the life of the
    /// link: cancelled when the link drops, and watched — a helper that ends, returning or
    /// panicking, is the link lost (a panic is also a driver bug, ERROR `driver_contract_violation`).
    pub fn spawn<F>(&mut self, task: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.helpers.spawn(Watched { task: Box::pin(task), handle: self.handle.clone() }.in_current_span());
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

    /// The bridge's instance.
    pub fn instance(&self) -> &str {
        self.handle.instance()
    }
}

#[cfg(test)]
mod tests;

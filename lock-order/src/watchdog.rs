//! The watchdog thread (no-hang spec §13.9, ruling 7 as revised): it reports every lock wait
//! that has gone on past its class's threshold, and never aborts. A plain thread, independent of
//! the runtime, so blocked workers cannot silence it. It also logs the order checker's cycles, so
//! nothing is logged from inside an acquisition. Runs in every build.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// How long a wait goes on before it is reported, for a class with no threshold of its own.
pub const DEFAULT_REPORT_AFTER: Duration = Duration::from_secs(60);

const TICK: Duration = Duration::from_millis(100);

/// A wait reported as long.
#[derive(Debug, Clone)]
pub struct LongWait {
    pub class: &'static str,
    /// Where the waiting acquisition was made.
    pub site: &'static Location<'static>,
    pub waited: Duration,
    /// Where the lock is held (debug builds; empty in release).
    pub holders: Vec<String>,
}

struct Wait {
    class: &'static str,
    site: &'static Location<'static>,
    instance: usize,
    since: Instant,
    reported: bool,
}

fn waits() -> &'static Mutex<HashMap<u64, Wait>> {
    static WAITS: OnceLock<Mutex<HashMap<u64, Wait>>> = OnceLock::new();
    WAITS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn thresholds() -> &'static RwLock<HashMap<&'static str, Duration>> {
    static T: OnceLock<RwLock<HashMap<&'static str, Duration>>> = OnceLock::new();
    T.get_or_init(|| RwLock::new(HashMap::new()))
}

fn long() -> &'static Mutex<Vec<LongWait>> {
    static LONG: OnceLock<Mutex<Vec<LongWait>>> = OnceLock::new();
    LONG.get_or_init(|| Mutex::new(Vec::new()))
}

/// How long `class`'s waits may go on before they are reported. A class that legitimately holds
/// for minutes (a template rebake, a config reload) gets a longer threshold, with the reason in
/// its registry row.
pub fn set_report_after(class: &'static str, after: Duration) {
    thresholds().write().unwrap_or_else(|p| p.into_inner()).insert(class, after);
}

/// How many reported waits [`long_waits`] keeps: the latest ones. The WARN is the record; this is
/// for a test harness, and must not grow for the life of a server that reports a slow holder
/// every few minutes.
pub const LONG_WAITS_KEPT: usize = 256;

/// The latest waits reported (at most [`LONG_WAITS_KEPT`]) — for a test harness.
pub fn long_waits() -> Vec<LongWait> {
    long().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

fn threshold(class: &'static str) -> Duration {
    thresholds().read().unwrap_or_else(|p| p.into_inner()).get(class).copied().unwrap_or(DEFAULT_REPORT_AFTER)
}

static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A registered wait; ends when dropped — on success, or when the acquisition is cancelled.
pub(crate) struct WaitGuard(Option<u64>);

impl WaitGuard {
    pub(crate) fn none() -> Self {
        WaitGuard(None)
    }

    pub(crate) fn begin(class: &'static str, site: &'static Location<'static>, instance: usize) -> Self {
        let mut g = WaitGuard(None);
        g.start(class, site, instance);
        g
    }

    pub(crate) fn start(&mut self, class: &'static str, site: &'static Location<'static>, instance: usize) {
        if self.0.is_some() {
            return;
        }
        ensure_thread();
        let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        waits().lock().unwrap_or_else(|p| p.into_inner()).insert(id, Wait { class, site, instance, since: Instant::now(), reported: false });
        self.0 = Some(id);
    }
}

impl Drop for WaitGuard {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            waits().lock().unwrap_or_else(|p| p.into_inner()).remove(&id);
        }
    }
}

/// `fut`, an inner acquisition, registered as a wait only if it does not complete at once — so an
/// uncontended acquisition costs nothing here.
pub(crate) async fn watched<F: std::future::Future>(
    class: &'static str,
    site: &'static Location<'static>,
    instance: usize,
    fut: F,
) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let mut wait = WaitGuard::none();
    std::future::poll_fn(|cx| match fut.as_mut().poll(cx) {
        std::task::Poll::Ready(v) => std::task::Poll::Ready(v),
        std::task::Poll::Pending => {
            wait.start(class, site, instance);
            std::task::Poll::Pending
        }
    })
    .await
}

#[allow(unused_variables)]
fn holders(instance: usize) -> Vec<String> {
    #[cfg(debug_assertions)]
    return crate::checker::holders_of(instance).into_iter().map(|s| s.to_string()).collect();
    #[cfg(not(debug_assertions))]
    Vec::new()
}

/// How long after a failed start an acquisition path tries to start the thread again.
const RETRY_AFTER: Duration = Duration::from_secs(1);

/// Whether the watchdog thread runs, and when starting it last failed. Marked started only once a
/// spawn has succeeded: a failed spawn (thread creation under resource pressure) is retried, not
/// remembered as done.
struct Starter {
    running: AtomicBool,
    last_failure: Mutex<Option<Instant>>,
}

impl Starter {
    const fn new() -> Self {
        Starter { running: AtomicBool::new(false), last_failure: Mutex::new(None) }
    }

    fn start(&self, retry_after: Duration, spawn: impl FnOnce() -> std::io::Result<()>) -> std::io::Result<()> {
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }
        let mut last = self.last_failure.lock().unwrap_or_else(|p| p.into_inner());
        if self.running.load(Ordering::Acquire) {
            return Ok(());
        }
        if last.is_some_and(|at| at.elapsed() < retry_after) {
            return Err(std::io::Error::other("the lock-order watchdog thread failed to start moments ago"));
        }
        match spawn() {
            Ok(()) => {
                self.running.store(true, Ordering::Release);
                *last = None;
                Ok(())
            }
            Err(e) => {
                *last = Some(Instant::now());
                Err(e)
            }
        }
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }
}

static STARTER: Starter = Starter::new();

fn spawn_thread() -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("lock-order-watchdog".into())
        .spawn(|| loop {
            std::thread::sleep(TICK);
            tick();
        })
        .map(drop)
}

/// From an acquisition path: start the thread if it is not running. Says nothing on failure —
/// nothing is logged from inside an acquisition — and retries at most once per `RETRY_AFTER`.
pub(crate) fn ensure_thread() {
    let _ = STARTER.start(RETRY_AFTER, spawn_thread);
}

/// Start the watchdog thread now. A program calls this at startup — before resource pressure can
/// make thread creation fail — and reports the error if it does: without the thread, long lock
/// waits and recorded cycles go unreported. Retried here at once, and from acquisition paths at
/// most once a second, until it starts.
pub fn start_watchdog() -> std::io::Result<()> {
    STARTER.start(Duration::ZERO, spawn_thread)
}

/// Whether the watchdog thread is running.
pub fn watchdog_running() -> bool {
    STARTER.running()
}

/// Push `item`, dropping the oldest past `cap`.
fn keep_latest<T>(kept: &mut Vec<T>, item: T, cap: usize) {
    if kept.len() >= cap {
        kept.remove(0);
    }
    kept.push(item);
}

#[cfg(test)]
mod kept_tests {
    /// Review finding (Claude, 2026-10-02): the record of reported waits grew for a server's life.
    #[test]
    fn the_record_keeps_only_the_latest() {
        let mut kept = Vec::new();
        for i in 0..1000 {
            super::keep_latest(&mut kept, i, 256);
        }
        assert_eq!(kept.len(), 256);
        assert_eq!(kept.first(), Some(&744));
        assert_eq!(kept.last(), Some(&999));
    }
}

#[cfg(test)]
mod starter_tests {
    use super::*;
    use std::cell::Cell;

    /// Review finding (Codex, 2026-10-02): a failed spawn was discarded and the `Once` completed,
    /// so the thread never existed and was never retried — silently. Now a failure is returned,
    /// the starter stays unstarted, and a later attempt spawns again.
    #[test]
    fn a_failed_start_is_reported_and_retried() {
        let s = Starter::new();
        let spawns = Cell::new(0);
        let failing = || {
            spawns.set(spawns.get() + 1);
            Err(std::io::Error::other("no threads left"))
        };
        assert!(s.start(Duration::from_secs(60), failing).is_err(), "the failure is returned");
        assert!(!s.running(), "a failed start is not a start");
        assert!(s.start(Duration::from_secs(60), || unreachable!("retried inside the window")).is_err());
        assert_eq!(spawns.get(), 1);
        let ok = || {
            spawns.set(spawns.get() + 1);
            Ok(())
        };
        assert!(s.start(Duration::ZERO, ok).is_ok(), "a retry past the window spawns again");
        assert!(s.running());
        assert!(s.start(Duration::ZERO, || unreachable!("started once")).is_ok());
        assert_eq!(spawns.get(), 2);
    }
}

fn tick() {
    #[cfg(debug_assertions)]
    for cycle in crate::checker::take_pending() {
        tracing::error!(
            kind = "lock_order_cycle",
            cycle = %cycle,
            "lock order cycle: two locks waited on in both orders, or one waited on again while held"
        );
    }
    let now = Instant::now();
    let due: Vec<(&'static str, &'static Location<'static>, usize, Duration)> = {
        let mut map = waits().lock().unwrap_or_else(|p| p.into_inner());
        map.values_mut()
            .filter(|w| !w.reported && now.duration_since(w.since) >= threshold(w.class))
            .map(|w| {
                w.reported = true;
                (w.class, w.site, w.instance, now.duration_since(w.since))
            })
            .collect()
    };
    for (class, site, instance, waited) in due {
        let holders = holders(instance);
        tracing::warn!(
            kind = "lock_wait_long",
            class,
            site = %site,
            waited_ms = waited.as_millis() as u64,
            holders = %holders.join(", "),
            "lock wait past its class's threshold — its holder, or what the holder waits on, may be stuck"
        );
        keep_latest(&mut long().lock().unwrap_or_else(|p| p.into_inner()), LongWait { class, site, waited, holders }, LONG_WAITS_KEPT);
    }
}

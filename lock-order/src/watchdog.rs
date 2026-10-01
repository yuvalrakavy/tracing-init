//! The watchdog thread (no-hang spec §13.9, ruling 7 as revised): it reports every lock wait
//! that has gone on past its class's threshold, and never aborts. A plain thread, independent of
//! the runtime, so blocked workers cannot silence it. It also logs the order checker's cycles, so
//! nothing is logged from inside an acquisition. Runs in every build.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::{Mutex, Once, OnceLock, RwLock};
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

/// Every wait reported so far — for a test harness.
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

pub(crate) fn ensure_thread() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("lock-order-watchdog".into()).spawn(|| loop {
            std::thread::sleep(TICK);
            tick();
        });
    });
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
        long().lock().unwrap_or_else(|p| p.into_inner()).push(LongWait { class, site, waited, holders });
    }
}

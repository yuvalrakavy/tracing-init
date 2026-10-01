//! The production escape (no-hang spec §13.9, ruling 7): every wrapped tokio lock acquisition
//! waits under a watchdog — an ERROR at a third of its class's bound, the installed wedge handler
//! past it. Runs in every build: an order no test took can still deadlock in production, and a
//! loud abort and restart is the ruled answer, never a silent hang.

use std::collections::HashMap;
use std::future::Future;
use std::panic::Location;
use std::sync::{OnceLock, RwLock};
use std::time::Duration;

/// How long an acquisition of a class with no bound of its own may wait.
pub const DEFAULT_BOUND: Duration = Duration::from_secs(120);

/// A lock wait past its class's bound.
#[derive(Debug, Clone)]
pub struct Wedge {
    pub class: &'static str,
    /// Where the waiting acquisition was made.
    pub site: &'static Location<'static>,
    pub waited: Duration,
    pub bound: Duration,
    /// Where the lock is held now (debug builds; empty in release).
    pub holders: Vec<String>,
}

type Handler = std::sync::Arc<dyn Fn(&Wedge) + Send + Sync>;

fn bounds() -> &'static RwLock<HashMap<&'static str, Option<Duration>>> {
    static BOUNDS: OnceLock<RwLock<HashMap<&'static str, Option<Duration>>>> = OnceLock::new();
    BOUNDS.get_or_init(|| RwLock::new(HashMap::new()))
}

fn handler() -> &'static RwLock<Option<Handler>> {
    static HANDLER: OnceLock<RwLock<Option<Handler>>> = OnceLock::new();
    HANDLER.get_or_init(|| RwLock::new(None))
}

/// Set `class`'s bound. `None`: its waits are bounded elsewhere (the store lock's own limits), and
/// the watchdog stays out.
pub fn set_bound(class: &'static str, bound: Option<Duration>) {
    bounds().write().unwrap_or_else(|p| p.into_inner()).insert(class, bound);
}

/// Install what runs when a wait passes its bound. Store's flushes its stores and aborts (ruling 3,
/// kernel-internal). Without one, the process aborts after the ERROR. A handler that returns lets
/// the wait go on — for tests.
pub fn set_wedge_handler(h: impl Fn(&Wedge) + Send + Sync + 'static) {
    *handler().write().unwrap_or_else(|p| p.into_inner()) = Some(std::sync::Arc::new(h));
}

fn bound_of(class: &'static str) -> Option<Duration> {
    match bounds().read().unwrap_or_else(|p| p.into_inner()).get(class) {
        Some(b) => *b,
        None => Some(DEFAULT_BOUND),
    }
}

#[allow(unused_variables)]
fn holders(instance: usize) -> Vec<String> {
    #[cfg(debug_assertions)]
    return crate::checker::holders_of(instance).into_iter().map(|s| s.to_string()).collect();
    #[cfg(not(debug_assertions))]
    Vec::new()
}

/// Wait for `fut`, the inner lock's acquisition, under `class`'s bound.
pub async fn bounded<F: Future>(class: &'static str, site: &'static Location<'static>, instance: usize, fut: F) -> F::Output {
    let Some(bound) = bound_of(class) else { return fut.await };
    let started = tokio::time::Instant::now();
    let mut fut = std::pin::pin!(fut);
    // Pinned once and polled by reference: an expiry drops only the timeout, never the queued
    // acquisition, so its place in the lock's queue survives every warning.
    if let Ok(v) = tokio::time::timeout_at(started + bound / 3, fut.as_mut()).await {
        return v;
    }
    let waiting = holders(instance);
    tracing::error!(
        kind = "lock_wait_wedged",
        class,
        site = %site,
        waited_ms = (bound / 3).as_millis() as u64,
        bound_ms = bound.as_millis() as u64,
        holders = %waiting.join(", "),
        "lock wait past a third of its bound — its holder, or what the holder waits on, is stuck"
    );
    let mut deadline = started + bound;
    loop {
        if let Ok(v) = tokio::time::timeout_at(deadline, fut.as_mut()).await {
            return v;
        }
        let wedge = Wedge { class, site, waited: started.elapsed(), bound, holders: holders(instance) };
        // Cloned out, so the handler runs with no lock of this module held.
        let installed = handler().read().unwrap_or_else(|p| p.into_inner()).clone();
        match installed {
            Some(h) => h(&wedge),
            None => {
                tracing::error!(kind = "lock_wait_wedged", class, site = %site, "lock wait past its bound: aborting");
                std::process::abort();
            }
        }
        deadline += bound;
    }
}

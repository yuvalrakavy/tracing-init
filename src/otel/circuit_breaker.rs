//! Circuit breaker wrapper for OTel exporters.
//!
//! Silently drops exports when the collector is unreachable, avoiding
//! repeated error messages from the batch processor. State transitions
//! are logged once via `eprintln!`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

fn now_timestamp() -> String {
    // Use chrono if available, otherwise fall back to a simple format
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let hours = (secs % 86400) / 3600;
    let mins = (secs % 3600) / 60;
    let s = secs % 60;
    format!("{hours:02}:{mins:02}:{s:02}")
}

use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};
use opentelemetry_sdk::Resource;

/// The process's circuit state, registered by `init()` so [`crate::telemetry_loss`] can
/// report availability beside the loss latch.
///
/// A global rather than a field on `TracingGuard`: the `Arc` is cloned into both exporter
/// wrappers and then MOVED into the beacon listener, and the guard never held it, so there
/// was no handle to read. `OnceLock` keeps the first registration — a second `init()` in one
/// process is already refused by `tracing` itself.
static REGISTERED: std::sync::OnceLock<Arc<CircuitState>> = std::sync::OnceLock::new();

/// Record the circuit built by `init()`. Ignores a second call.
pub(crate) fn register(state: Arc<CircuitState>) {
    let _ = REGISTERED.set(state);
}

/// The registered circuit, or `None` when OTel was never initialized.
pub fn registered() -> Option<&'static Arc<CircuitState>> {
    REGISTERED.get()
}

// Circuit states
const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// Shared circuit breaker state, used by both span and log exporters.
///
/// Uses atomics so the batch processor threads can read/write without locks.
pub struct CircuitState {
    state: AtomicU8,
    failure_count: AtomicU32,
    failure_threshold: u32,
    /// Epoch instant used to compute relative timestamps stored in `last_probe_ms`.
    epoch: Instant,
    /// Milliseconds since `epoch` when the circuit last opened or was last probed.
    last_probe_ms: AtomicU64,
    reprobe_interval_ms: u64,
    /// Guard to ensure the offline message is printed exactly once per offline period.
    has_logged_offline: AtomicBool,
    /// Application name for log messages.
    app_name: String,
    /// Exports attempted and failed, monotonic for the life of the process.
    ///
    /// `failure_count` above is the CONSECUTIVE count the breaker acts on, and it is zeroed
    /// at four sites (`record_success`, `open_circuit`, `force_close`, `force_open`), so
    /// after an outage that recovers it reads 0 and nothing records that anything failed.
    export_failures_total: AtomicU64,
    /// Batches dropped without an export attempt because the circuit was open (or a
    /// half-open probe was already in flight). Monotonic.
    batches_discarded_total: AtomicU64,
    /// Unix-epoch milliseconds of the first recorded failure; `0` means none yet. Set once.
    first_failure_at_ms: AtomicU64,
}

impl fmt::Debug for CircuitState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state_name = match self.state.load(Ordering::Relaxed) {
            CLOSED => "Closed",
            OPEN => "Open",
            HALF_OPEN => "HalfOpen",
            _ => "Unknown",
        };
        f.debug_struct("CircuitState")
            .field("state", &state_name)
            .field("failure_count", &self.failure_count.load(Ordering::Relaxed))
            .field(
                "export_failures_total",
                &self.export_failures_total.load(Ordering::Relaxed),
            )
            .field(
                "batches_discarded_total",
                &self.batches_discarded_total.load(Ordering::Relaxed),
            )
            .field(
                "first_failure_at_ms",
                &self.first_failure_at_ms.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl CircuitState {
    /// Create a new circuit breaker state.
    ///
    /// - `failure_threshold`: consecutive failures before opening the circuit.
    /// - `reprobe_interval_secs`: seconds to wait in Open state before probing.
    pub fn new(failure_threshold: u32, reprobe_interval_secs: u64, app_name: &str) -> Self {
        Self {
            state: AtomicU8::new(CLOSED),
            failure_count: AtomicU32::new(0),
            failure_threshold,
            epoch: Instant::now(),
            last_probe_ms: AtomicU64::new(0),
            reprobe_interval_ms: reprobe_interval_secs * 1000,
            has_logged_offline: AtomicBool::new(false),
            app_name: app_name.to_string(),
            export_failures_total: AtomicU64::new(0),
            batches_discarded_total: AtomicU64::new(0),
            first_failure_at_ms: AtomicU64::new(0),
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Returns `true` if the export should proceed, `false` if it should be dropped.
    ///
    /// The discard is counted HERE, in a wrapper over the decision, rather than at each
    /// `false` inside it. The gate has three `false` returns today and a fourth added later
    /// would silently stop being counted; this shape cannot miss one. Both exporter wrappers
    /// call this one function, so spans and logs are covered by a single site.
    fn should_export(&self) -> bool {
        let allowed = self.evaluate_export_gate();
        if !allowed {
            self.batches_discarded_total.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }

    /// The circuit's own decision, with no accounting — see [`Self::should_export`].
    fn evaluate_export_gate(&self) -> bool {
        let state = self.state.load(Ordering::Acquire);
        match state {
            CLOSED => true,
            OPEN => {
                let elapsed = self.now_ms() - self.last_probe_ms.load(Ordering::Relaxed);
                if elapsed >= self.reprobe_interval_ms {
                    // Transition to HalfOpen — only one thread wins
                    if self
                        .state
                        .compare_exchange(OPEN, HALF_OPEN, Ordering::AcqRel, Ordering::Relaxed)
                        .is_ok()
                    {
                        return true;
                    }
                }
                false
            }
            HALF_OPEN => {
                // Only one probe at a time; others drop
                false
            }
            _ => false,
        }
    }

    /// Record a successful export.
    fn record_success(&self) {
        let prev = self.state.swap(CLOSED, Ordering::Release);
        self.failure_count.store(0, Ordering::Relaxed);
        if prev != CLOSED {
            // Only clear the offline flag on a genuine reconnection
            // (transition from Open/HalfOpen to Closed), not on every
            // successful export while already Closed.
            self.has_logged_offline.store(false, Ordering::Relaxed);
            eprintln!(
                "[{}] [{}] OTel collector online, sending traces",
                now_timestamp(),
                self.app_name
            );
        }
    }

    /// Record a failed export.
    fn record_failure(&self) {
        // Monotonic, before the state machine below: this counts every failed export
        // attempt, including one that leaves the circuit's own consecutive count reset.
        self.export_failures_total.fetch_add(1, Ordering::Relaxed);
        let _ = self.first_failure_at_ms.compare_exchange(
            0,
            crate::loss::unix_millis_now(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );

        let state = self.state.load(Ordering::Acquire);
        match state {
            CLOSED => {
                let count = self.failure_count.fetch_add(1, Ordering::Relaxed) + 1;
                if count >= self.failure_threshold {
                    self.open_circuit();
                }
            }
            HALF_OPEN => {
                // Probe failed — back to Open
                self.open_circuit();
            }
            _ => {}
        }
    }

    fn open_circuit(&self) {
        self.state.store(OPEN, Ordering::Release);
        self.failure_count.store(0, Ordering::Relaxed);
        self.last_probe_ms.store(self.now_ms(), Ordering::Relaxed);
        // Log exactly once per offline period using atomic flag
        if !self.has_logged_offline.swap(true, Ordering::AcqRel) {
            let secs = self.reprobe_interval_ms / 1000;
            eprintln!(
                "[{}] [{}] OTel collector not online. Start the collector and traces will begin flowing within {secs}s",
                now_timestamp(), self.app_name
            );
        }
    }

    /// Exports attempted and failed, for the life of the process. Never reset.
    pub fn export_failures_total(&self) -> u64 {
        self.export_failures_total.load(Ordering::Relaxed)
    }

    /// Batches discarded without an export attempt, for the life of the process.
    pub fn batches_discarded_total(&self) -> u64 {
        self.batches_discarded_total.load(Ordering::Relaxed)
    }

    /// When the first export failure was recorded, or `None` if none ever was.
    pub fn first_failure_at(&self) -> Option<std::time::SystemTime> {
        crate::loss::system_time_from_millis(self.first_failure_at_ms.load(Ordering::Relaxed))
    }

    /// `true` while the circuit is not closed — exports are being discarded.
    pub fn is_open(&self) -> bool {
        self.state.load(Ordering::Acquire) != CLOSED
    }

    /// Force the circuit closed (e.g. from beacon ONLINE message).
    pub fn force_close(&self) {
        let prev = self.state.swap(CLOSED, Ordering::Release);
        self.failure_count.store(0, Ordering::Relaxed);
        self.has_logged_offline.store(false, Ordering::Relaxed);
        if prev != CLOSED {
            eprintln!(
                "[{}] [{}] OTel collector online, sending traces",
                now_timestamp(),
                self.app_name
            );
        }
    }

    /// Force the circuit open (e.g. from beacon OFFLINE message).
    pub fn force_open(&self) {
        self.state.store(OPEN, Ordering::Release);
        self.failure_count.store(0, Ordering::Relaxed);
        self.last_probe_ms.store(self.now_ms(), Ordering::Relaxed);
        if !self.has_logged_offline.swap(true, Ordering::AcqRel) {
            let secs = self.reprobe_interval_ms / 1000;
            eprintln!(
                "[{}] [{}] OTel collector not online. Start the collector and traces will begin flowing within {secs}s",
                now_timestamp(), self.app_name
            );
        }
    }
}

// ── Span Exporter Wrapper ──

/// Wraps a `SpanExporter`, silently dropping spans when the circuit is open.
///
/// Generic over the inner exporter type because `SpanExporter` is no longer
/// dyn-compatible in opentelemetry_sdk 0.31 (its `export` method returns
/// `impl Future`). Mirrors the pattern used for `LogExporter` below.
pub struct CircuitBreakerSpanExporter<E> {
    inner: E,
    state: Arc<CircuitState>,
}

impl<E: fmt::Debug> fmt::Debug for CircuitBreakerSpanExporter<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerSpanExporter")
            .field("inner", &self.inner)
            .field("state", &self.state)
            .finish()
    }
}

impl<E> CircuitBreakerSpanExporter<E> {
    pub fn new(inner: E, state: Arc<CircuitState>) -> Self {
        Self { inner, state }
    }
}

impl<E: SpanExporter> SpanExporter for CircuitBreakerSpanExporter<E> {
    fn export(
        &self,
        batch: Vec<SpanData>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let should = self.state.should_export();
        let state = self.state.clone();

        async move {
            if !should {
                return Ok(());
            }

            match self.inner.export(batch).await {
                Ok(()) => {
                    state.record_success();
                    Ok(())
                }
                Err(_) => {
                    state.record_failure();
                    Ok(()) // Never propagate errors
                }
            }
        }
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&mut self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

// ── Log Exporter Wrapper ──

/// Wraps a `LogExporter`, silently dropping logs when the circuit is open.
/// Generic over the inner exporter type because `LogExporter` is not
/// dyn-compatible (its `export` method returns `impl Future`).
pub struct CircuitBreakerLogExporter<E> {
    inner: E,
    state: Arc<CircuitState>,
}

impl<E: fmt::Debug> fmt::Debug for CircuitBreakerLogExporter<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerLogExporter")
            .field("inner", &self.inner)
            .field("state", &self.state)
            .finish()
    }
}

impl<E> CircuitBreakerLogExporter<E> {
    pub fn new(inner: E, state: Arc<CircuitState>) -> Self {
        Self { inner, state }
    }
}

impl<E: LogExporter> LogExporter for CircuitBreakerLogExporter<E> {
    fn export(
        &self,
        batch: LogBatch<'_>,
    ) -> impl std::future::Future<Output = OTelSdkResult> + Send {
        let should = self.state.should_export();
        let state = self.state.clone();

        async move {
            if !should {
                return Ok(());
            }

            match self.inner.export(batch).await {
                Ok(()) => {
                    state.record_success();
                    Ok(())
                }
                Err(_) => {
                    state.record_failure();
                    Ok(()) // Never propagate errors
                }
            }
        }
    }

    fn shutdown(&self) -> OTelSdkResult {
        // LogExporter::shutdown is `&self` in opentelemetry_sdk 0.31; the
        // inner exporter is also `&self`-shutdown so we just delegate.
        self.inner.shutdown()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

/// A child module, so the private gate and recorders can be driven directly. Widening them
/// to `pub(crate)` for tests would move production surface for test convenience.
#[cfg(test)]
mod tests {
    use super::*;

    /// V22 / A16 — the counters survive the circuit closing again, and `first_failure_at`
    /// is set once.
    ///
    /// The last assertion is the point of the whole addition: after an outage that
    /// RECOVERED, `failure_count` reads 0 — it is the consecutive count the breaker acts on
    /// and is zeroed whenever the circuit closes — so before these fields, a run that lost
    /// its collector for a while and got it back ended with no evidence at all.
    #[test]
    fn availability_counters_survive_a_recovery() {
        let circuit = CircuitState::new(2, 30, "test");

        circuit.record_failure();
        let first = circuit
            .first_failure_at()
            .expect("a failure records its instant");
        circuit.record_failure(); // reaches the threshold and opens the circuit
        assert!(
            circuit.is_open(),
            "two failures at threshold 2 must open the circuit"
        );

        circuit.record_success(); // the collector came back
        assert!(!circuit.is_open(), "a success must close the circuit again");

        assert_eq!(
            circuit.export_failures_total(),
            2,
            "both failures must survive the recovery"
        );
        assert_eq!(
            circuit.first_failure_at(),
            Some(first),
            "first_failure_at is set once and must not move to the later failure"
        );
        assert_eq!(
            circuit.failure_count.load(Ordering::Relaxed),
            0,
            "the pre-existing consecutive count is back to zero — this is the blindness \
             the monotonic counters exist to fix, and if it ever stops being zero here, \
             the assertions above stop being interesting"
        );
    }

    /// Every discard is counted, and only a discard is.
    #[test]
    fn discards_are_counted_while_the_circuit_is_open() {
        let circuit = CircuitState::new(3, 30, "test");
        assert_eq!(circuit.batches_discarded_total(), 0);

        assert!(circuit.should_export(), "a closed circuit exports");
        assert_eq!(
            circuit.batches_discarded_total(),
            0,
            "an allowed export must not be counted as a discard"
        );

        circuit.force_open();
        assert!(!circuit.should_export());
        assert!(!circuit.should_export());
        assert_eq!(
            circuit.batches_discarded_total(),
            2,
            "each refused batch counts exactly once"
        );
    }

    /// A14 — availability loss is not telemetry loss. An unreachable collector is a
    /// condition the breaker handles by design; it must never set the drop latch, which
    /// means "records that no longer exist".
    ///
    /// Sound in parallel because nothing in this test binary installs the latch layer
    /// against the process-wide latch: `tests/loss_tests.rs` uses local instances precisely
    /// so this assertion keeps its meaning.
    #[test]
    fn availability_is_not_loss() {
        let circuit = CircuitState::new(1, 30, "test");
        circuit.record_failure();
        circuit.force_open();
        assert!(!circuit.should_export());
        assert!(circuit.export_failures_total() > 0 && circuit.batches_discarded_total() > 0);

        let loss = crate::loss::telemetry_loss();
        assert!(
            !loss.any_dropped(),
            "a failing, open exporter must not report dropped telemetry"
        );
    }
}

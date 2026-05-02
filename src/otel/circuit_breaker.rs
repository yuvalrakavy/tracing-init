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
        }
    }

    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }

    /// Returns `true` if the export should proceed, `false` if it should be dropped.
    fn should_export(&self) -> bool {
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
            eprintln!("[{}] [{}] OTel collector online, sending traces", now_timestamp(), self.app_name);
        }
    }

    /// Record a failed export.
    fn record_failure(&self) {
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

    /// Force the circuit closed (e.g. from beacon ONLINE message).
    pub fn force_close(&self) {
        let prev = self.state.swap(CLOSED, Ordering::Release);
        self.failure_count.store(0, Ordering::Relaxed);
        self.has_logged_offline.store(false, Ordering::Relaxed);
        if prev != CLOSED {
            eprintln!("[{}] [{}] OTel collector online, sending traces", now_timestamp(), self.app_name);
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

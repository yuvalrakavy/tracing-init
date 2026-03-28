//! Circuit breaker wrapper for OTel exporters.
//!
//! Silently drops exports when the collector is unreachable, avoiding
//! repeated error messages from the batch processor. State transitions
//! are logged once via `eprintln!`.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Instant;

use futures_util::future::BoxFuture;
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
    pub fn new(failure_threshold: u32, reprobe_interval_secs: u64) -> Self {
        Self {
            state: AtomicU8::new(CLOSED),
            failure_count: AtomicU32::new(0),
            failure_threshold,
            epoch: Instant::now(),
            last_probe_ms: AtomicU64::new(0),
            reprobe_interval_ms: reprobe_interval_secs * 1000,
            has_logged_offline: AtomicBool::new(false),
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
        self.has_logged_offline.store(false, Ordering::Relaxed);
        if prev != CLOSED {
            eprintln!("OTel collector online, sending traces");
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
                "OTel collector not online. Start the collector and traces will begin flowing within {secs}s"
            );
        }
    }

    /// Force the circuit closed (e.g. from beacon ONLINE message).
    pub fn force_close(&self) {
        let prev = self.state.swap(CLOSED, Ordering::Release);
        self.failure_count.store(0, Ordering::Relaxed);
        self.has_logged_offline.store(false, Ordering::Relaxed);
        if prev != CLOSED {
            eprintln!("OTel collector online, sending traces");
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
                "OTel collector not online. Start the collector and traces will begin flowing within {secs}s"
            );
        }
    }
}

// ── Span Exporter Wrapper ──

/// Wraps a `SpanExporter`, silently dropping spans when the circuit is open.
pub struct CircuitBreakerSpanExporter {
    inner: Box<dyn SpanExporter>,
    state: Arc<CircuitState>,
}

impl fmt::Debug for CircuitBreakerSpanExporter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CircuitBreakerSpanExporter")
            .field("state", &self.state)
            .finish()
    }
}

impl CircuitBreakerSpanExporter {
    pub fn new(inner: Box<dyn SpanExporter>, state: Arc<CircuitState>) -> Self {
        Self { inner, state }
    }
}

impl SpanExporter for CircuitBreakerSpanExporter {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, OTelSdkResult> {
        if !self.state.should_export() {
            return Box::pin(std::future::ready(Ok(())));
        }

        let state = self.state.clone();
        let fut = self.inner.export(batch);

        Box::pin(async move {
            match fut.await {
                Ok(()) => {
                    state.record_success();
                    Ok(())
                }
                Err(_) => {
                    state.record_failure();
                    Ok(()) // Never propagate errors
                }
            }
        })
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

    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

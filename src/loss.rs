//! Telemetry loss: did this process silently drop spans or logs?
//!
//! The OpenTelemetry SDK keeps its dropped-span and dropped-log counts private and reports
//! loss only through two internal `tracing` events — one the first time it drops, and one
//! carrying exact totals at shutdown. This module latches the first of those, so a process
//! can be ASKED whether it lost telemetry instead of the question being answered by
//! whoever remembers to run the right log query.
//!
//! # Why a latch and not a count
//!
//! Deriving "how many were lost" from closed − received − in-flight is wrong under
//! parent-based sampling, pending work and shutdown timeouts. The SDK reports only the
//! FIRST drop until shutdown, so a latch is exactly as much as can be known while the
//! process runs. The SDK's shutdown events still carry exact totals for forensics.
//!
//! # Prerequisite, easy to disarm by accident
//!
//! The SDK's internal events are gated behind the `opentelemetry` crate's `internal-logs`
//! feature, which is ON by default (`default = [… "internal-logs" …]`, and `internal-logs`
//! is what routes them through `tracing` at all). A dependency graph that takes
//! `opentelemetry` with `default-features = false` and does not re-add it makes this latch
//! permanently silent — it would report "no loss" forever, which is worse than reporting
//! nothing. If you disable default features there, re-enable `internal-logs`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{Event, Subscriber};
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Target the SDK emits its internal warnings on.
///
/// `otel_warn!` expands to `warn!(name: …, target: env!("CARGO_PKG_NAME"), …)`, and the
/// macro is invoked inside `opentelemetry_sdk`, so that is the target — not
/// `opentelemetry`, where the macro is defined.
pub const SDK_TARGET: &str = "opentelemetry_sdk";

/// Metadata name of the SDK's first-span-drop warning.
///
/// Emitted from both batch span processor implementations (`trace/span_processor.rs` and
/// `trace/span_processor_with_async_runtime.rs`) in opentelemetry_sdk 0.31.
pub const SPAN_DROPPING_STARTED: &str = "BatchSpanProcessor.SpanDroppingStarted";

/// Metadata name of the SDK's first-log-drop warning, from both batch log processor
/// implementations.
pub const LOG_DROPPING_STARTED: &str = "BatchLogProcessor.LogDroppingStarted";

/// Unix-epoch milliseconds now, saturating at 0 before the epoch.
pub(crate) fn unix_millis_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Rebuild a `SystemTime` from stored milliseconds; `0` is the "never happened" sentinel.
pub(crate) fn system_time_from_millis(ms: u64) -> Option<SystemTime> {
    (ms != 0).then(|| UNIX_EPOCH + Duration::from_millis(ms))
}

/// A pair of set-once flags recording when telemetry loss began.
///
/// An instance rather than bare statics so the matching rule can be tested without touching
/// process-global state, which would make those tests order-dependent on each other.
#[derive(Debug)]
pub struct LossLatch {
    spans_dropped_since_ms: AtomicU64,
    logs_dropped_since_ms: AtomicU64,
}

impl Default for LossLatch {
    fn default() -> Self {
        Self::new()
    }
}

impl LossLatch {
    pub const fn new() -> Self {
        Self {
            spans_dropped_since_ms: AtomicU64::new(0),
            logs_dropped_since_ms: AtomicU64::new(0),
        }
    }

    /// Latch on an event's metadata NAME. Returns `true` if this call set a flag.
    ///
    /// Matching is on the name alone because the caller is already filtered to
    /// [`SDK_TARGET`], and deliberately NOT on the message: the SDK's event message is
    /// empty and its human text rides in a `message` field, so message matching finds
    /// nothing, while a look-alike record from another crate carrying the same text must
    /// not latch.
    pub fn note_event_name(&self, name: &str) -> bool {
        let cell = match name {
            SPAN_DROPPING_STARTED => &self.spans_dropped_since_ms,
            LOG_DROPPING_STARTED => &self.logs_dropped_since_ms,
            _ => return false,
        };
        cell.compare_exchange(0, unix_millis_now(), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// When span loss began, or `None` if no span drop has been observed.
    pub fn spans_dropped_since(&self) -> Option<SystemTime> {
        system_time_from_millis(self.spans_dropped_since_ms.load(Ordering::Relaxed))
    }

    /// When log loss began, or `None` if no log drop has been observed.
    pub fn logs_dropped_since(&self) -> Option<SystemTime> {
        system_time_from_millis(self.logs_dropped_since_ms.load(Ordering::Relaxed))
    }
}

static GLOBAL_LATCH: LossLatch = LossLatch::new();

/// The process-wide latch the installed layer writes to.
pub fn global_latch() -> &'static LossLatch {
    &GLOBAL_LATCH
}

/// What this process knows about telemetry loss and exporter availability.
///
/// Loss and availability are separate axes and are never folded together: an unreachable
/// collector is an availability problem that the circuit breaker handles by design, while a
/// full queue is telemetry that no longer exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TelemetryLoss {
    /// When the SDK first dropped a span, if it ever did.
    pub spans_dropped_since: Option<SystemTime>,
    /// When the SDK first dropped a log record, if it ever did.
    pub logs_dropped_since: Option<SystemTime>,
    /// Exports attempted and failed, for the life of the process. `0` without OTel.
    pub export_failures_total: u64,
    /// Batches discarded while the circuit was open. `0` without OTel.
    pub batches_discarded_total: u64,
    /// When the first export failure was recorded.
    pub first_failure_at: Option<SystemTime>,
    /// Whether the exporter circuit is currently open; `None` when OTel is not initialized.
    pub circuit_open: Option<bool>,
}

impl TelemetryLoss {
    /// `true` if the SDK dropped spans or logs at any point. This is the question that
    /// invalidates a measurement; availability alone does not.
    pub fn any_dropped(&self) -> bool {
        self.spans_dropped_since.is_some() || self.logs_dropped_since.is_some()
    }
}

/// What this process knows about telemetry loss, now.
///
/// Never cleared once set, and safe to call whether or not OTel was initialized.
pub fn telemetry_loss() -> TelemetryLoss {
    let latch = global_latch();

    #[cfg(feature = "otel")]
    let circuit = crate::otel::circuit_breaker::registered();
    #[cfg(not(feature = "otel"))]
    let circuit: Option<&'static std::sync::Arc<()>> = None;

    #[cfg(feature = "otel")]
    let (export_failures_total, batches_discarded_total, first_failure_at, circuit_open) =
        match circuit {
            Some(c) => (
                c.export_failures_total(),
                c.batches_discarded_total(),
                c.first_failure_at(),
                Some(c.is_open()),
            ),
            None => (0, 0, None, None),
        };
    #[cfg(not(feature = "otel"))]
    let (export_failures_total, batches_discarded_total, first_failure_at, circuit_open) = {
        let _ = circuit;
        (0, 0, None, None)
    };

    TelemetryLoss {
        spans_dropped_since: latch.spans_dropped_since(),
        logs_dropped_since: latch.logs_dropped_since(),
        export_failures_total,
        batches_discarded_total,
        first_failure_at,
        circuit_open,
    }
}

/// The layer that watches for the SDK's drop warnings.
///
/// Writes to [`global_latch`] by default, so the answer outlives the subscriber and any
/// guard. It holds the latch by reference rather than reaching for the global directly so a
/// test can point one at its own instance: a test that exercised this layer against the
/// process-wide latch would set a flag other tests assert is clear, and the instrument this
/// module exists to make trustworthy would be the thing with order-dependent tests.
#[derive(Debug, Clone, Copy)]
pub struct LossLatchLayer {
    latch: &'static LossLatch,
}

impl Default for LossLatchLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl LossLatchLayer {
    /// Writes to the process-wide latch — what `init()` installs.
    pub fn new() -> Self {
        Self {
            latch: global_latch(),
        }
    }

    /// Writes to a caller-supplied latch instead of the process-wide one.
    pub fn with_latch(latch: &'static LossLatch) -> Self {
        Self { latch }
    }
}

impl<S: Subscriber> Layer<S> for LossLatchLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        self.latch.note_event_name(event.metadata().name());
    }
}

/// The latch's own filter: exactly the SDK's target, at WARN.
///
/// `Targets::new()` denies everything not named, so this layer sees the SDK's warnings and
/// nothing else in the process.
pub fn sdk_warn_filter() -> Targets {
    Targets::new().with_target(SDK_TARGET, LevelFilter::WARN)
}

/// The latch layer behind its own filter — the only correct way to install it.
///
/// It must NOT be attached plain, and must NOT sit behind a destination's filter. A plain
/// layer reports no max-level hint, which drags the whole subscriber's hint to TRACE and
/// undoes static level skipping in every process; and a plain layer that narrows `enabled`
/// disables those callsites for every OTHER layer. Behind a destination filter instead, a
/// filter that excludes `opentelemetry_sdk` WARN leaves the latch permanently clear — the
/// silent-zero failure this module exists to remove.
/// The bounds are what `Layer::boxed` needs at the call site: the filtered layer is
/// `Filtered<LossLatchLayer, Targets, S>`, and both halves are `Send + Sync`, so the only
/// extra requirement is a `'static` subscriber.
pub fn loss_latch_layer<S>() -> impl Layer<S> + Send + Sync + 'static
where
    S: Subscriber + for<'a> LookupSpan<'a> + 'static,
{
    LossLatchLayer::new().with_filter(sdk_warn_filter())
}

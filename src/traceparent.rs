//! W3C `traceparent` ↔ `tracing` span helpers.
//!
//! Carry trace context across transports that are not natively
//! instrumented — message queues (e.g. MQTT v5 user properties), custom
//! wire protocols, scheduled or queued work:
//!
//! 1. At the cause site, render the active context with [`current`] and
//!    attach the string to the outgoing message.
//! 2. At the handling site, create a span for the work and re-parent it
//!    with [`set_remote_parent`] **before the span is first entered**.
//!    Every event emitted inside then carries the originating trace id
//!    (GELF `_trace_id`, OTLP parent linkage).
//!
//! Both functions are no-ops returning `None`/`false` unless the
//! `tracing-opentelemetry` layer is installed (the `o` destination):
//! span contexts are only valid, and `set_parent` only has somewhere to
//! write, when spans carry OpenTelemetry data.

use std::collections::HashMap;

use opentelemetry::propagation::TextMapPropagator;
use opentelemetry::trace::TraceContextExt;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Render the current span's trace context as a W3C `traceparent` string
/// (`00-<32 hex trace id>-<16 hex parent span id>-<2 hex flags>`).
///
/// Returns `None` when no valid OpenTelemetry context is active — no
/// OTel layer installed, or no sampled span current. Unlike helpers that
/// carry only the trace id, the full traceparent preserves the real
/// parent span id, so trace views show an intact parent link instead of
/// a dangling synthesized one.
pub fn current() -> Option<String> {
    let ctx = tracing::Span::current().context();
    let span_ref = ctx.span();
    let sc = span_ref.span_context();
    if !sc.is_valid() {
        return None;
    }
    Some(format!(
        "00-{:032x}-{:016x}-{:02x}",
        sc.trace_id(),
        sc.span_id(),
        sc.trace_flags().to_u8()
    ))
}

/// Re-parent `span` onto the remote context described by a W3C
/// `traceparent` string (as produced by [`current`] or any compliant
/// peer).
///
/// Must be called BEFORE the span is first entered: `set_parent` only
/// populates the Builder-state parent context that the GELF layer and
/// the OTLP exporter read; on an already-activated span it has no
/// effect.
///
/// Returns `true` when the string parsed to a valid remote span context
/// (the re-parenting itself additionally requires the OTel layer to be
/// installed — without it the call is a silent no-op).
pub fn set_remote_parent(span: &tracing::Span, traceparent: &str) -> bool {
    let propagator = TraceContextPropagator::new();
    let mut carrier = HashMap::with_capacity(1);
    carrier.insert("traceparent".to_string(), traceparent.to_string());
    let context = propagator.extract(&carrier);
    let valid = context.span().span_context().is_valid();
    if valid {
        span.set_parent(context);
    }
    valid
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn current_is_none_without_otel_layer() {
        // No subscriber / no OTel layer: there is no valid span context.
        assert_eq!(current(), None);
    }

    #[test]
    fn set_remote_parent_rejects_garbage() {
        let span = tracing::Span::none();
        assert!(!set_remote_parent(&span, "not-a-traceparent"));
        assert!(!set_remote_parent(&span, ""));
        // Valid shape but all-zero ids — invalid per the W3C spec.
        assert!(!set_remote_parent(
            &span,
            "00-00000000000000000000000000000000-0000000000000000-01"
        ));
    }

    #[test]
    fn set_remote_parent_accepts_valid_traceparent() {
        let span = tracing::Span::none();
        assert!(set_remote_parent(
            &span,
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        ));
    }

    #[test]
    fn round_trip_preserves_trace_id() {
        use opentelemetry::trace::TracerProvider as _;

        // A real (exporter-less) SDK tracer makes span contexts valid.
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("traceparent-test");
        let layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            let root = tracing::info_span!("cause_site");
            let tp = root.in_scope(current).expect("valid context inside root span");
            assert!(tp.starts_with("00-"));
            let trace_id = &tp[3..35];

            // Detached work: new span, re-parented before first enter.
            let detached = tracing::info_span!("handling_site");
            assert!(set_remote_parent(&detached, &tp));
            let detached_tp = detached.in_scope(current).expect("valid context in re-parented span");
            assert_eq!(
                &detached_tp[3..35],
                trace_id,
                "re-parented span must continue the originating trace"
            );
            assert_ne!(
                detached_tp, tp,
                "child renders its own span id, not the parent's"
            );
        });
    }
}

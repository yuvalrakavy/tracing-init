//! Tests for the telemetry-loss latch.
//!
//! The thing under test is an instrument whose failure mode is a confident silent zero, so
//! every assertion here has a control that makes it vacuous if the mechanism is removed.
//!
//! None of these touch the process-wide latch: the matching rule is tested on a local
//! `LossLatch`, and the layer test points the layer at its own leaked instance. That is what
//! lets `availability_is_not_loss` in `otel::circuit_breaker` assert the global flag is
//! clear without depending on which test ran first.

use tracing::Subscriber as _;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Layer as _;

use crate::loss::{
    loss_latch_layer, sdk_warn_filter, LossLatch, LossLatchLayer, LOG_DROPPING_STARTED, SDK_TARGET,
    SPAN_DROPPING_STARTED,
};

/// V3 — the latch sets on each of the SDK's two drop names, and STAYS at the first time.
#[test]
fn the_latch_sets_on_each_drop_name_and_keeps_the_first_time() {
    let latch = LossLatch::new();
    assert!(latch.spans_dropped_since().is_none());
    assert!(latch.logs_dropped_since().is_none());

    assert!(
        latch.note_event_name(SPAN_DROPPING_STARTED),
        "first span drop must latch"
    );
    let first = latch.spans_dropped_since().expect("span latch is set");

    // The SDK emits its warning only once until shutdown, but a second call must not move
    // the recorded instant even if it ever did: "since" is when loss BEGAN.
    assert!(
        !latch.note_event_name(SPAN_DROPPING_STARTED),
        "a second drop must not re-latch"
    );
    assert_eq!(latch.spans_dropped_since(), Some(first));

    // Spans and logs are independent flags; the span drop above must not have set logs.
    assert!(
        latch.logs_dropped_since().is_none(),
        "a span drop must not set the log flag"
    );
    assert!(latch.note_event_name(LOG_DROPPING_STARTED));
    assert!(latch.logs_dropped_since().is_some());
}

/// V3 — look-alikes must not latch. This is the control that makes the test above mean
/// something: a latch that fires on anything would pass that one and fail this.
#[test]
fn look_alike_records_do_not_latch() {
    let latch = LossLatch::new();

    // The SDK's own human text. It rides in a `message` FIELD while the event's message is
    // empty and the identifier is the metadata name — so matching on message text would
    // both latch here and miss every real drop.
    assert!(!latch.note_event_name("BatchSpanProcessor dropped a Span due to queue full"));
    // The SHUTDOWN totals event, a different name from the same processor.
    assert!(!latch.note_event_name("BatchSpanProcessor.SpansDropped"));
    assert!(!latch.note_event_name("BatchLogProcessor.LogsDropped"));
    assert!(!latch.note_event_name(""));
    assert!(!latch.note_event_name("event src/lib.rs:42"));

    assert!(latch.spans_dropped_since().is_none());
    assert!(latch.logs_dropped_since().is_none());
}

/// The layer actually fires on a real-shaped SDK event, and its filter keeps another
/// crate's identically-named event out. Verifying the instrument can fire is the whole
/// point: a latch that never fires is indistinguishable from "nothing was lost".
#[test]
fn the_layer_latches_the_sdk_event_and_its_filter_excludes_other_targets() {
    let latch: &'static LossLatch = Box::leak(Box::new(LossLatch::new()));
    let subscriber = tracing_subscriber::registry()
        .with(LossLatchLayer::with_latch(latch).with_filter(sdk_warn_filter()));

    tracing::subscriber::with_default(subscriber, || {
        // Shaped exactly like `otel_warn!`: metadata name carries the identifier, the
        // message is empty, the target is the emitting SDK crate.
        tracing::event!(
            name: "BatchSpanProcessor.SpanDroppingStarted",
            target: "opentelemetry_sdk",
            tracing::Level::WARN,
            ""
        );
        // Same name, another crate's target — must be filtered out before the layer sees it.
        tracing::event!(
            name: "BatchLogProcessor.LogDroppingStarted",
            target: "some_other_crate",
            tracing::Level::WARN,
            ""
        );
    });

    assert!(
        latch.spans_dropped_since().is_some(),
        "the SDK's span-drop event must reach the latch"
    );
    assert!(
        latch.logs_dropped_since().is_none(),
        "an identically-named event on another target must not latch"
    );
}

/// V21 / A11 — attaching the latch must not widen the subscriber's level hint.
///
/// Asserted on `Subscriber::max_level_hint()` rather than `LevelFilter::current()`: it is
/// the same property, tested deterministically and without depending on dispatcher globals,
/// so this test runs in parallel with everything else.
#[test]
fn the_latch_does_not_widen_the_level_hint() {
    let filtered = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(LevelFilter::INFO))
        .with(loss_latch_layer());
    assert_eq!(
        filtered.max_level_hint(),
        Some(LevelFilter::INFO),
        "the latch's own Targets filter reports WARN, which is narrower than the \
         destination's INFO, so the subscriber's hint must stay INFO"
    );

    // The control. A plain layer reports NO hint, and a `Vec`/`Layered` with one hint-less
    // member has no hint at all — which makes the process behave as TRACE and undoes static
    // level skipping in every crate. This is why the layer is never attached bare.
    let plain = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_filter(LevelFilter::INFO))
        .with(LossLatchLayer::new());
    assert_eq!(
        plain.max_level_hint(),
        None,
        "a plain latch layer must be shown to erase the hint, or the test above proves nothing"
    );
}

/// The filter admits the SDK's target and denies everything else — `Targets::new()` starts
/// from deny, which is what keeps this layer from seeing the whole process.
#[test]
fn the_filter_names_only_the_sdk_target() {
    let rendered = sdk_warn_filter().to_string();
    assert!(
        rendered.contains(SDK_TARGET),
        "filter must name the SDK target: {rendered}"
    );
    assert!(
        rendered.contains("warn") || rendered.contains("WARN"),
        "at WARN: {rendered}"
    );
}

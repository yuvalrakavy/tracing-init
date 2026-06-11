use serde_json::Map;
use std::net::UdpSocket;
use std::time::Duration;
use tracing_subscriber::layer::SubscriberExt;

/// Bind a loopback UDP socket and a GelfLayer pointed at it; run `f` with
/// that layer as the thread-default subscriber; return the received GELF
/// records as parsed JSON, in order.
fn capture_gelf(f: impl FnOnce()) -> Vec<serde_json::Value> {
    let socket = UdpSocket::bind("127.0.0.1:0").expect("bind loopback");
    socket
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("set timeout");
    let addr = socket.local_addr().expect("local addr");

    let layer = crate::gelf::GelfLayer::new(&addr.to_string(), vec![], Some("test".into()))
        .expect("gelf layer");
    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, f);

    let mut records = Vec::new();
    let mut buf = [0u8; 65536];
    while let Ok(n) = socket.recv(&mut buf) {
        records.push(serde_json::from_slice(&buf[..n]).expect("valid GELF JSON"));
    }
    records
}

#[test]
fn test_span_chain_field_inheritance() {
    let records = capture_gelf(|| {
        let outer = tracing::info_span!("outer", panel_id = 7, shared = "outer");
        let _o = outer.enter();
        let inner = tracing::info_span!("inner", shared = "inner");
        let _i = inner.enter();
        tracing::info!("nested event");
    });

    assert_eq!(records.len(), 1);
    let rec = &records[0];
    // Outer span's field is inherited by the event in the inner span.
    assert_eq!(rec["_span_panel_id"], 7);
    // Inner span's value wins on a name collision.
    assert_eq!(rec["_span_shared"], "inner");
    // The span name stays the innermost.
    assert_eq!(rec["_span_name"], "inner");
}

#[test]
fn test_span_record_after_creation() {
    let records = capture_gelf(|| {
        let span = tracing::info_span!("s", late = tracing::field::Empty);
        let _e = span.enter();
        span.record("late", 42);
        tracing::info!("after record");
    });

    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["_span_late"], 42);
}

#[test]
fn test_log_bridge_events_are_normalized() {
    // LogTracer forwards `log` records to the current (thread-default)
    // tracing dispatcher. Global init — ignore AlreadyInit from other tests.
    let _ = tracing_log::LogTracer::init();

    let records = capture_gelf(|| {
        log::warn!(target: "fontdb", "failed to load font 'X'");
    });

    assert_eq!(records.len(), 1);
    let rec = &records[0];
    // The real emitter target, not the bridge's static "log".
    assert_eq!(rec["_target"], "fontdb");
    assert_eq!(rec["level"], 4);
    assert_eq!(rec["short_message"], "failed to load font 'X'");
    // The bridge's carrier fields are not emitted as GELF fields.
    let obj = rec.as_object().unwrap();
    assert!(
        !obj.keys().any(|k| k.starts_with("_log.")),
        "carrier fields leaked: {:?}",
        obj.keys().collect::<Vec<_>>()
    );
}

#[test]
fn test_gelf_includes_service_name() {
    let mut fields = Map::new();
    crate::gelf::add_service_field(&mut fields, Some("my-service"));
    assert_eq!(
        fields.get("_service").and_then(|v| v.as_str()),
        Some("my-service")
    );
}

#[test]
fn test_gelf_includes_target() {
    let mut fields = Map::new();
    crate::gelf::add_metadata_fields(
        &mut fields,
        Some("my_crate::module"),
        Some("src/main.rs"),
        Some(42),
    );
    assert_eq!(
        fields.get("_target").and_then(|v| v.as_str()),
        Some("my_crate::module")
    );
    assert_eq!(
        fields.get("_file").and_then(|v| v.as_str()),
        Some("src/main.rs")
    );
    assert_eq!(fields.get("_line").and_then(|v| v.as_u64()), Some(42));
}

#[test]
fn test_gelf_service_name_none() {
    let mut fields = Map::new();
    crate::gelf::add_service_field(&mut fields, None);
    assert!(!fields.contains_key("_service"));
}

//! Malformed `Desired` and `Command` payloads (Bridge Runtime spec §9): each is answered on
//! `Error` (`"reason":"malformed"`, naming what it can) with one INFO `validation_rejected`, never a
//! panic, and the bridge goes on.

use mqtt_bridge_conformance::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn malformed_desired_and_command_payloads_are_answered_on_error() {
    let h = Harness::start_with("malformed", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "temp", json!(20))).await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await);
    let t = h.topics.clone();
    let malformed: Vec<(String, &[u8])> = vec![
        (t.desired("t1", "temp"), b"not json"),
        (t.desired("t1", "temp"), br#"{"an": "object"}"#),
        (t.desired_target("t1"), b"[1, 2]"),
        (t.desired_target("t1"), b"{}"),
        (t.command(), b"not json"),
        (t.command(), br#"{"target": "t1"}"#),
        (t.command(), br#"{"command": "assume"}"#),
        (format!("{}/Desired/{}", t.root(), t.instance()), b"1"),
    ];
    for (topic, payload) in &malformed {
        assert!(h.send_raw(topic, payload));
    }
    // A conflict rule that names neither side is malformed too.
    let bad_conflict = SendOptions::default().user_property("conflict", "Whoever");
    assert!(h.broker.send_with(&t.desired("t1", "temp"), "22", &bad_conflict));
    // A target-level write or a command the broker replays as retained was left there by mistake:
    // replayed at every restart, it would act again each time.
    assert!(h.broker.send_with(&t.desired_target("t1"), r#"{"temp": 30}"#, &SendOptions::default().retained()));
    assert!(h.broker.send_with(&t.command(), r#"{"target": "t1", "command": "pulse"}"#, &SendOptions::default().retained()));
    // An unknown command is unsupported.
    assert!(h.command(json!({"target": "t1", "command": "dance"}), None));
    let expected = malformed.len() + 3;
    assert!(
        h.eventually(BOUND, |h| h.errors_for("malformed").len() == expected).await,
        "not every malformed payload was answered on Error: {:#?}",
        h.errors()
    );
    let errors = h.errors_for("malformed");
    assert_eq!((errors[0]["target"].as_str(), errors[0]["property"].as_str()), (Some("t1"), Some("temp")), "{:?}", errors[0]);
    assert!(
        h.eventually(BOUND, |h| h.errors_for("unsupported").len() == 1).await,
        "the unknown command was not answered: {:?}",
        h.errors()
    );
    assert_eq!(h.log.of_kind("validation_rejected").len(), expected, "{:#?}", h.log.records());
    assert!(h.device.applies().is_empty(), "a malformed request was applied");
    assert!(h.device.calls_of(Op::Execute).is_empty(), "a malformed command was executed");
    // The bridge goes on.
    assert!(h.desire("t1", "temp", json!(22), Req::default()));
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "the bridge stopped working after malformed payloads"
    );
    assert!(h.stop().await);
}

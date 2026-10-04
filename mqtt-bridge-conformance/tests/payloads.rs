//! Malformed `Desired` and `Command` payloads (Bridge Runtime spec §9): each is answered on
//! `Error` (`"reason":"malformed"`, naming what it can) with one INFO `validation_rejected`, never a
//! panic, and the bridge goes on.

use std::time::Duration;

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
        (t.config("t1"), b"not json"),
    ];
    for (topic, payload) in &malformed {
        assert!(h.send_raw(topic, payload));
    }
    // A conflict rule that names neither side is malformed too.
    let bad_conflict = SendOptions::default().user_property("conflict", "Whoever");
    assert!(h.broker.publish(&t.desired("t1", "temp"), "22", &bad_conflict));
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

/// A retained `Config` of 20 KiB — an IR code set — reaches `configure` and the bridge stays on
/// the broker; one past the bridge's advertised maximum packet size (1 MiB) is discarded by the
/// broker, and the bridge stays all the same (review B3: rumqttc's 10 KiB limit was not
/// advertised, so the broker replayed a larger retained message on every subscribe and rumqttc
/// dropped the connection each time — a reconnect loop).
#[tokio::test(flavor = "multi_thread")]
async fn a_large_retained_config_is_taken_and_one_past_the_limit_is_left_at_the_broker() {
    let big = "x".repeat(20 * 1024);
    let huge = "y".repeat(1536 * 1024);
    let h = Harness::start_with("sizes", acknowledged(), quick_timing(), move |broker, topics, device| {
        device.set("t1", "temp", json!(20));
        broker.retain(&topics.config("ir"), json!({ "codes": big }).to_string(), &SendOptions::default());
        broker.retain(&topics.config("huge"), json!({ "codes": huge }).to_string(), &SendOptions::default());
    })
    .await;
    let configured = |h: &Harness, target: &str| h.device.calls_of(Op::Configure).iter().any(|c| c.target.as_deref() == Some(target));
    assert!(
        h.eventually(BOUND, |h| configured(h, "ir")).await,
        "the 20 KiB config never reached configure; {} connections",
        h.broker.connections()
    );
    h.settle(Duration::from_secs(2)).await;
    assert_eq!(h.broker.connections(), 1, "a large retained message made the bridge reconnect");
    assert!(!configured(&h, "huge"), "a config past the advertised maximum packet size reached the driver");
    assert!(h.desire("t1", "temp", json!(22), Req::default()));
    assert!(h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await, "the bridge stopped working");
    assert!(h.stop().await);
}

/// A retained `Command` or target-level `Desired` published while the bridge listens is refused
/// as malformed, as a replayed one is: the bridge subscribes to them retain-as-published, so the
/// broker keeps the flag on a live delivery. A live retained property write — every write the
/// Store makes — is a request like any other: its own subscription clears the flag on a live
/// delivery (review C10: every live retained publish arrived unflagged, and a retained `pulse`
/// executed).
#[tokio::test(flavor = "multi_thread")]
async fn a_live_retained_command_or_target_write_is_refused_and_a_live_retained_property_write_is_taken() {
    let h = Harness::start_with("liveretained", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "temp", json!(20))).await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await);
    let t = h.topics.clone();
    let retained = SendOptions::default().retained();
    assert!(h.broker.publish(&t.command(), r#"{"target": "t1", "command": "pulse"}"#, &retained));
    assert!(h.broker.publish(&t.desired_target("t1"), r#"{"temp": 30}"#, &retained));
    assert!(
        h.eventually(BOUND, |h| h.errors_for("malformed").len() == 2).await,
        "a live retained command or target-level write was not refused: {:?}",
        h.errors()
    );
    assert!(h.desire("t1", "temp", json!(22), Req { expiry: Some(60), conflict: Some("DeviceWins") }));
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "a live retained property write was not taken: {:?}",
        h.device.calls()
    );
    assert!(h.device.calls_of(Op::Execute).is_empty(), "a live retained command was executed");
    assert!(h.device.applies().iter().all(|(_, v)| *v != json!({"temp": 30})), "a live retained target-level write was applied");
    assert!(
        h.device.calls_of(Op::Read).iter().all(|c| c.target.as_deref() != Some("t1")),
        "a live property write was checked as a replay: {:?}",
        h.device.calls()
    );
    assert!(h.stop().await);
}

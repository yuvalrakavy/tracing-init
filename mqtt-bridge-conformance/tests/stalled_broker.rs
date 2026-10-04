//! Reports never wait (Bridge Runtime spec §4.2, §5; mqtt_ac re-review C1): with the broker
//! stalled — its acknowledgements held, the client's request channel full, the session parked on a
//! publish — the device side still does its work.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use serde_json::json;

/// A request held while the device was down is applied when the device returns, though the broker
/// is stalled throughout the catch-up (which reports the device's status and state).
#[tokio::test(flavor = "multi_thread")]
async fn the_device_side_works_while_the_broker_is_stalled() {
    let h = Harness::start_with("stalled", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set_up(false);
    })
    .await;
    assert!(h.desire("t1", "temp", json!(22), Req { expiry: Some(60), conflict: Some("RequestWins") }));
    h.settle(Duration::from_millis(300)).await;

    // Stall the broker, and fill the session's path to it: each malformed command is an Error to
    // publish, and past the in-flight limit and the request channel the session waits.
    h.broker.hold_acks();
    for i in 0..1000 {
        assert!(h.send_raw(&h.topics.command(), format!("not json {i}").as_bytes()));
    }
    assert!(
        h.eventually(BOUND, |h| h.broker.held_acks() >= 10).await,
        "the broker's acknowledgements were never held: the path is not saturated — or the pump waits on the session"
    );
    h.settle(Duration::from_millis(300)).await;
    // The barrier: a request sent now is read only by a session that is not parked on a full
    // request channel. A target-level write is never retained, so nothing brings it back later.
    let probe = SendOptions::default().expiry(60).user_property("conflict", "RequestWins");
    assert!(h.broker.publish(&h.topics.desired_target("t2"), r#"{"temp": 5}"#, &probe));
    h.settle(Duration::from_millis(300)).await;

    h.device.set_up(true);
    assert!(
        h.eventually(Duration::from_secs(5), |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "with the broker stalled the device worker did not apply the held request: it waits on MQTT"
    );
    h.settle(Duration::from_millis(300)).await;
    assert!(
        h.device.applies().iter().all(|(target, _)| target != "t2"),
        "a request sent during the stall was read: the session was not parked on a full request channel, so this test proves nothing: {:?}",
        h.device.applies()
    );
    h.broker.release_acks();
    assert!(
        h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await,
        "the bridge did not recover with the broker — the pump waits on the session, or its session does"
    );
    assert!(h.stop().await);
}

/// A broker that answers PINGREQ but withholds every PUBACK never fails a poll, so nothing in
/// rumqttc notices it: the bridge ends the session once nothing it asked has been acknowledged for
/// twice the keep-alive, and makes a fresh one (Bridge Runtime review C1).
#[tokio::test(flavor = "multi_thread")]
async fn a_broker_that_acknowledges_nothing_is_left_for_a_fresh_session() {
    let h = Harness::start_with("unacked", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "temp", json!(20))).await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await);
    h.broker.hold_acks();
    // Something to publish, which the broker takes and never acknowledges.
    h.device.set("t1", "temp", json!(21));
    assert!(h.command(json!({"target": "t1", "command": "refresh"}), None));
    assert!(
        h.eventually(Duration::from_secs(25), |h| h.broker.connections() >= 2).await,
        "the bridge stayed for 25 s on a broker that acknowledged nothing: {} connection(s); the log: {:#?}",
        h.broker.connections(),
        h.log.records()
    );
    let lost = h.log.of_kind("connection_lost");
    assert!(
        lost.len() == 1 && lost[0].field("error").is_some_and(|e| e.contains("acknowledged nothing")),
        "the stall was not one outage said for what it was: {:#?}",
        h.log.records()
    );
    h.broker.release_acks();
    assert!(
        h.eventually(BOUND, |h| h.broker.retained_on(&h.topics.state("t1")).is_some_and(|s| &s[..] == br#"{"temp":21}"#)).await,
        "the fresh session did not publish the model again: {:?}",
        h.broker.retained_on(&h.topics.state("t1"))
    );
    assert!(h.stop().await);
}

/// A broker that stops reading leaves rumqttc waiting in its flush, inside the poll, where its
/// keep-alive cannot fire: the bridge bounds the poll, ends the session and makes a fresh one
/// (Bridge Runtime review C1).
#[tokio::test(flavor = "multi_thread")]
async fn a_broker_that_stops_reading_is_left_for_a_fresh_session() {
    let h = Harness::start("unread", acknowledged(), quick_timing()).await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await);
    h.broker.stop_reading();
    // More than the sockets' buffers hold: twelve states of a megabyte each.
    let blob = "x".repeat(1 << 20);
    for i in 0..12 {
        let values: mqtt_bridge_kit::PropertyMap = [("blob".to_owned(), json!(format!("{i}{blob}")))].into_iter().collect();
        assert!(h.device.report(&format!("big{i}"), values));
    }
    assert!(
        h.eventually(Duration::from_secs(25), |h| h.broker.connections() >= 2).await,
        "the bridge stayed for 25 s on a broker that stopped reading; the log: {:#?}",
        h.log.of_kind("connection_lost")
    );
    assert!(
        h.eventually(BOUND, |h| h.broker.retained_on(&h.topics.state("big11")).is_some()).await,
        "the fresh session did not publish the model again"
    );
    assert!(h.stop().await);
}

/// A request's expiry counts from its arrival at the bridge, not from when the session got to it: a
/// command that waited past its expiry behind a stalled broker — the session parked on a full
/// request channel — is never executed (review C7: decoding granted it its whole expiry again).
#[tokio::test(flavor = "multi_thread")]
async fn a_requests_expiry_counts_from_its_arrival_not_from_when_the_session_reads_it() {
    let h = Harness::start_with("expiry-arrival", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "temp", json!(20))).await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await);
    h.broker.hold_acks();
    for i in 0..1000 {
        assert!(h.send_raw(&h.topics.command(), format!("not json {i}").as_bytes()));
    }
    assert!(h.eventually(BOUND, |h| h.broker.held_acks() >= 10).await, "the path is not saturated");
    h.settle(Duration::from_millis(300)).await;
    // One second to live, arriving while the session is parked.
    let command = json!({"target": "t1", "command": "pulse"}).to_string();
    assert!(h.broker.publish(&h.topics.command(), command, &SendOptions::default().expiry(1)));
    h.settle(Duration::from_millis(2000)).await;
    h.broker.release_acks();
    h.settle(Duration::from_millis(1500)).await;
    assert!(h.device.calls_of(Op::Execute).is_empty(), "a command executed past its expiry, counted from when the session read it");
    assert!(h.stop().await);
}

/// An `assume` read past its expiry — it waited behind a stalled broker — is refused, not taken: the
/// model is not overwritten by a stale assumption (the R1 re-gate's R9: only momentary commands
/// carried their deadline).
#[tokio::test(flavor = "multi_thread")]
async fn an_assume_past_its_expiry_is_refused_not_taken() {
    let h = Harness::start_with("assume-expiry", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "temp", json!(20))).await;
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await);
    assert!(h.eventually(BOUND, |h| h.state("t1").is_some()).await);
    h.broker.hold_acks();
    for i in 0..1000 {
        assert!(h.send_raw(&h.topics.command(), format!("not json {i}").as_bytes()));
    }
    assert!(h.eventually(BOUND, |h| h.broker.held_acks() >= 10).await, "the path is not saturated");
    h.settle(Duration::from_millis(300)).await;
    let assume = json!({"target": "t1", "command": "assume", "temp": 30}).to_string();
    assert!(h.broker.publish(&h.topics.command(), assume, &SendOptions::default().expiry(1)));
    h.settle(Duration::from_millis(2000)).await;
    h.broker.release_acks();
    assert!(
        h.eventually(Duration::from_secs(5), |h| h.errors_for("refused").iter().any(|e| e["command"] == "assume")).await,
        "an assume past its expiry was not refused: State {:?}, Errors {:?}",
        h.state("t1"),
        h.errors_for("refused")
    );
    assert_ne!(h.state("t1").map(|s| s["temp"].clone()), Some(json!(30)), "an assume past its expiry overwrote the model");
    assert!(h.stop().await);
}

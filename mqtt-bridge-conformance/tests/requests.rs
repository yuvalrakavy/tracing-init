//! Requests (Bridge Runtime spec §9; Store kit spec §4): a request while the device is down,
//! applied on recovery within its expiry and never after it; a momentary command while down; the
//! outage conflict both ways, after a device outage and after a bridge restart; a target-level
//! write reaching `apply` once with all values; a retraction before the device returns.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use serde_json::{json, Value};

const SETTLE: Duration = Duration::from_millis(600);

fn values(pairs: &[(&str, Value)]) -> Value {
    Value::Object(pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())
}

/// A device that starts down, with `t1` at temp 20.
fn down_at_20(_: &FakeBroker, _: &mqtt_bridge_kit::topics::Topics, device: &Device) {
    device.set("t1", "temp", json!(20));
    device.set_up(false);
}

async fn connected(h: &Harness) -> bool {
    h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await
}

/// A request made while the device is down is held, and applied when it is back, within its
/// expiry.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_while_the_device_is_down_is_applied_on_recovery_within_its_expiry() {
    let h = Harness::start_with("held", acknowledged(), quick_timing(), down_at_20).await;
    assert!(h.desire("t1", "temp", json!(22), Req { expiry: Some(60), conflict: Some("DeviceWins") }));
    h.settle(Duration::from_millis(300)).await;
    assert!(h.device.applies().is_empty(), "a request was applied to a device that is down");
    h.device.set_up(true);
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "the held request was never applied: {:?}",
        h.device.calls()
    );
    assert_eq!(h.device.applies(), vec![("t1".to_owned(), values(&[("temp", json!(22))]))]);
    let recovered = h.log.of_kind("device_recovered");
    assert!(
        recovered.len() == 1 && recovered[0].field("applied") == Some("1"),
        "the recovery did not count the applied request: {:#?}",
        h.log.records()
    );
    assert!(h.stop().await);
}

/// A request held past its expiry (its MQTT message expiry) is dropped: never applied after it.
#[tokio::test(flavor = "multi_thread")]
async fn a_request_held_past_its_expiry_is_never_applied() {
    let h = Harness::start_with("expired", acknowledged(), quick_timing(), down_at_20).await;
    assert!(h.desire("t1", "temp", json!(22), Req { expiry: Some(1), conflict: Some("RequestWins") }));
    h.settle(Duration::from_millis(1500)).await;
    h.device.set_up(true);
    assert!(connected(&h).await, "the device never connected");
    h.settle(SETTLE).await;
    assert!(h.device.applies().is_empty(), "a request was applied after its expiry: {:?}", h.device.applies());
    assert_eq!(h.device.get("t1", "temp"), Some(json!(20)));
    assert!(h.stop().await);
}

/// A momentary command while the device is down is refused at once — an `Error` naming it, an
/// INFO `command_refused` — and never executed later.
#[tokio::test(flavor = "multi_thread")]
async fn a_momentary_command_while_the_device_is_down_is_refused_and_never_executed() {
    let h = Harness::start_with("momentary", acknowledged(), quick_timing(), down_at_20).await;
    assert!(h.command(json!({"target": "t1", "command": "pulse"}), None));
    assert!(h.eventually(BOUND, |h| !h.errors_for("refused").is_empty()).await, "the refused command got no Error: {:?}", h.errors());
    let refused = &h.errors_for("refused")[0];
    assert_eq!((refused["target"].as_str(), refused["command"].as_str()), (Some("t1"), Some("pulse")));
    // Refused as it came, not queued and refused by the next failed connect.
    assert_eq!(refused["error"], "the device is not connected", "the command was not refused at once: {refused}");
    assert_eq!(h.log.of_kind("command_refused").len(), 1);
    h.device.set_up(true);
    assert!(connected(&h).await);
    h.settle(SETTLE).await;
    assert!(h.device.calls_of(Op::Execute).is_empty(), "a refused command was executed later");
    assert!(h.stop().await);
}

/// Bring `h`'s device (up, `t1` temp 20 and power false, observed) down, change temp to 18 at the
/// device during the outage, and request temp 22 and power true under `conflict`; then bring it
/// back.
async fn outage_with_a_change_at_the_device(h: &Harness, conflict: &'static str) {
    assert!(
        h.eventually(BOUND, |h| h.state("t1") == Some(json!({"temp": 20, "power": false}))).await,
        "the device's state was never observed"
    );
    h.device.set_up(false);
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await, "the outage was never seen");
    h.device.set("t1", "temp", json!(18));
    let req = Req { expiry: Some(60), conflict: Some(conflict) };
    assert!(h.desire("t1", "temp", json!(22), req));
    assert!(h.desire("t1", "power", json!(true), req));
    h.settle(Duration::from_millis(200)).await;
    h.device.set_up(true);
}

fn up_at_20(_: &FakeBroker, _: &mqtt_bridge_kit::topics::Topics, device: &Device) {
    device.set("t1", "temp", json!(20));
    device.set("t1", "power", json!(false));
}

/// The outage conflict, `DeviceWins`: a property changed at the device during the outage keeps the
/// device's value — the request is dropped and reported superseded; an unchanged one takes the
/// request.
#[tokio::test(flavor = "multi_thread")]
async fn the_outage_conflict_device_wins() {
    let h = Harness::start_with("devicewins", acknowledged(), quick_timing(), up_at_20).await;
    outage_with_a_change_at_the_device(&h, "DeviceWins").await;
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "power") == Some(json!(true))).await,
        "the unchanged property's request was not applied"
    );
    assert!(
        h.eventually(BOUND, |h| !h.errors_for("superseded").is_empty()).await,
        "no Error said the request was superseded: {:?}",
        h.errors()
    );
    let superseded = h.errors_for("superseded");
    assert_eq!(superseded.len(), 1, "{superseded:?}");
    assert_eq!((superseded[0]["target"].as_str(), superseded[0]["property"].as_str()), (Some("t1"), Some("temp")));
    assert_eq!(h.device.get("t1", "temp"), Some(json!(18)), "the change at the device was reverted");
    assert_eq!(
        h.device.applies(),
        vec![("t1".to_owned(), values(&[("power", json!(true))]))],
        "a held value the device already had was written again, or the superseded one was"
    );
    assert!(h.stop().await);
}

/// The outage conflict, `RequestWins`: the request is applied over the change at the device.
#[tokio::test(flavor = "multi_thread")]
async fn the_outage_conflict_request_wins() {
    let h = Harness::start_with("requestwins", acknowledged(), quick_timing(), up_at_20).await;
    outage_with_a_change_at_the_device(&h, "RequestWins").await;
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "the request did not win: {:?}",
        h.device.calls()
    );
    assert_eq!(h.device.get("t1", "power"), Some(json!(true)));
    assert!(h.errors_for("superseded").is_empty(), "a RequestWins request was superseded");
    assert!(h.stop().await);
}

/// After a bridge restart the conflict rule's "before" values are the bridge's own retained
/// `State`, read back at start: a request made while the bridge was away (retained at the broker)
/// meets a device changed meanwhile, and `DeviceWins` keeps the device's value.
#[tokio::test(flavor = "multi_thread")]
async fn the_outage_conflict_after_a_bridge_restart_uses_the_read_back() {
    let h = Harness::start_with("readback", acknowledged(), quick_timing(), |broker, topics, device| {
        // What the previous run published, and what was asked while it was away.
        broker.retain(&topics.state("t1"), json!({"temp": 20, "power": false}).to_string(), &SendOptions::default());
        let req = SendOptions::default().expiry(3600).user_property("conflict", "DeviceWins");
        broker.retain(&topics.desired("t1", "temp"), "22", &req);
        broker.retain(&topics.desired("t1", "power"), "true", &req);
        // Changed at the device while the bridge was away: temp only.
        device.set("t1", "temp", json!(18));
        device.set("t1", "power", json!(false));
        // The conflict check's read of t1 is slow: by the time it decides, the bridge's own state
        // (published after its CONNACK, temp 18) has come back from the broker, unflagged — and
        // must not have been taken for the read-back.
        device.answer(Op::Read, Some("t1"), Answer::Delay(Duration::from_millis(400)));
    })
    .await;
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "power") == Some(json!(true))).await,
        "the unchanged property's request was not applied"
    );
    assert!(
        h.eventually(BOUND, |h| !h.errors_for("superseded").is_empty()).await,
        "no Error said the request was superseded: {:?}; the calls: {:?}",
        h.errors(),
        h.device.calls().iter().map(|c| format!("{:?} {:?} {:?}", c.op, c.target, c.values)).collect::<Vec<_>>()
    );
    assert_eq!(h.device.get("t1", "temp"), Some(json!(18)), "a change made while the bridge was away was reverted");
    assert!(h.stop().await);
}

/// A target-level write reaches `apply` once, with all its values — its property copies, which the
/// Store publishes next, are not applied again.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_level_write_reaches_apply_once_with_all_values() {
    let h = Harness::start_with("atomic", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    assert!(h.desire_many("t1", json!({"power": true, "temp": 22}), Req { expiry: Some(60), conflict: Some("DeviceWins") }));
    assert!(h.eventually(BOUND, |h| !h.device.applies().is_empty()).await, "the target write was never applied");
    h.settle(SETTLE).await;
    assert_eq!(
        h.device.applies(),
        vec![("t1".to_owned(), values(&[("power", json!(true)), ("temp", json!(22))]))],
        "the target write did not reach apply once with all its values"
    );
    assert!(h.stop().await);
}

/// A request retracted before the device returns is never applied.
#[tokio::test(flavor = "multi_thread")]
async fn a_retraction_before_the_device_returns_is_never_applied() {
    let h = Harness::start_with("retracted", acknowledged(), quick_timing(), down_at_20).await;
    assert!(h.desire("t1", "temp", json!(22), Req { expiry: Some(60), conflict: Some("RequestWins") }));
    h.settle(Duration::from_millis(200)).await;
    assert!(h.retract("t1", "temp"));
    h.settle(Duration::from_millis(200)).await;
    h.device.set_up(true);
    assert!(connected(&h).await);
    h.settle(SETTLE).await;
    assert!(h.device.applies().is_empty(), "a retracted request was applied: {:?}", h.device.applies());
    assert!(h.stop().await);
}

const DEVICE_WINS: Req = Req { expiry: Some(3600), conflict: Some("DeviceWins") };

/// A request made during a broker outage — kept at the broker, replayed on the next connection —
/// is decided against what the bridge knew when it stopped listening, not the start-up read-back:
/// a value the Store itself set since start-up is no change at the device (review B1: it was
/// taken for one, and DeviceWins dropped every replay of a property changed since start-up).
#[tokio::test(flavor = "multi_thread")]
async fn a_request_made_during_a_broker_outage_is_decided_against_what_the_bridge_last_knew() {
    let h = Harness::start_with("blip", acknowledged(), quick_timing(), |broker, topics, device| {
        broker.retain(&topics.state("t1"), json!({"temp": 20}).to_string(), &SendOptions::default());
        device.set("t1", "temp", json!(20));
    })
    .await;
    assert!(connected(&h).await);
    // The Store sets 23; the device takes it; its State says 23; the Store retracts.
    assert!(h.desire("t1", "temp", json!(23), DEVICE_WINS));
    assert!(h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(23))).await);
    assert!(h.command(json!({"target": "t1", "command": "refresh"}), None));
    assert!(h.eventually(BOUND, |h| h.state("t1") == Some(json!({"temp": 23}))).await, "the bridge never saw the device at 23");
    assert!(h.retract("t1", "temp"));
    h.settle(Duration::from_millis(300)).await;
    // The broker goes away; meanwhile the Store asks for 25, kept at the broker.
    h.broker.refuse_connections(true);
    h.broker.drop_connection();
    h.settle(Duration::from_millis(300)).await;
    h.broker.retain(&h.topics.desired("t1", "temp"), "25", &SendOptions::default().expiry(3600).user_property("conflict", "DeviceWins"));
    h.broker.refuse_connections(false);
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(25)) || !h.errors_for("superseded").is_empty()).await,
        "nothing became of the replayed request"
    );
    assert!(
        h.errors_for("superseded").is_empty(),
        "a request made during a broker outage was superseded though nobody touched the device: {:?}",
        h.errors()
    );
    assert_eq!(h.device.get("t1", "temp"), Some(json!(25)));
    assert!(h.stop().await);
}

/// A link lost during the catch-up — after the proving read, before the held values are all
/// written — does not make the device's current values the next link's "before" values: a change
/// made at the device during the outage still counts (review M1).
#[tokio::test(flavor = "multi_thread")]
async fn a_link_lost_during_the_catch_up_does_not_forget_a_change_at_the_device() {
    let h = Harness::start_with("flap", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set("t2", "temp", json!(20));
    })
    .await;
    assert!(h.eventually(BOUND, |h| h.state("t2").is_some() && h.status().as_deref() == Some("connected")).await);
    h.device.set_up(false);
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await);
    // At the wall, during the outage: t2 to 18. Requests for both.
    h.device.set("t2", "temp", json!(18));
    assert!(h.desire("t1", "temp", json!(22), DEVICE_WINS));
    assert!(h.desire("t2", "temp", json!(22), DEVICE_WINS));
    h.settle(Duration::from_millis(200)).await;
    // The catch-up's first write loses the link.
    h.device.answer_times(Op::Apply, Some("t1"), Answer::Link, 1);
    h.device.set_up(true);
    assert!(h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await, "t1 was never applied: {:?}", h.device.calls());
    h.settle(Duration::from_millis(600)).await;
    assert_eq!(
        h.device.get("t2", "temp"),
        Some(json!(18)),
        "the change at the wall during the outage was reverted after a link lost during the catch-up: {:?}",
        h.errors()
    );
    assert!(h.errors_for("superseded").iter().any(|e| e["target"] == "t2"), "t2's request was not reported superseded: {:?}", h.errors());
    assert!(h.stop().await);
}

/// The conflict check applies to each value on its own: a live request taken together with a
/// replayed one for the same target is applied, never superseded for its company (review MINOR 3).
#[tokio::test(flavor = "multi_thread")]
async fn a_live_request_grouped_with_a_replayed_one_is_not_checked() {
    // Operations may take 3 s here: the busy write below must not cost the link.
    let timing = mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(3)), ..quick_timing() };
    let h = Harness::start_with("grouped", acknowledged(), timing, |broker, topics, device| {
        // What the previous run published; since then mode was changed at the device.
        broker.retain(&topics.state("t1"), json!({"temp": 20, "mode": "heat"}).to_string(), &SendOptions::default());
        device.set("t1", "temp", json!(20));
        device.set("t1", "mode", json!("auto"));
        device.set("t0", "x", json!(0));
    })
    .await;
    assert!(connected(&h).await);
    // Keep the worker busy while a replay and a live write for t1 arrive.
    h.device.answer_times(Op::Apply, Some("t0"), Answer::Delay(Duration::from_millis(1500)), 1);
    assert!(h.desire("t0", "x", json!(1), Req::default()));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Apply).is_empty()).await);
    // A replay (the broker brings back what was retained while the bridge was away)…
    h.broker.retain(&h.topics.desired("t1", "temp"), "22", &SendOptions::default().expiry(3600).user_property("conflict", "RequestWins"));
    let connects = h.broker.connections();
    h.broker.drop_connection();
    assert!(h.eventually(BOUND, |h| h.broker.connections() > connects).await);
    h.settle(Duration::from_millis(200)).await;
    // …and a live write, not retained, for the same target.
    assert!(h.broker.publish(
        &h.topics.desired("t1", "mode"),
        "\"cool\"",
        &SendOptions::default().expiry(3600).user_property("conflict", "DeviceWins")
    ));
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "mode") == Some(json!("cool")) || !h.errors_for("superseded").is_empty()).await,
        "nothing became of the live request"
    );
    assert!(h.errors_for("superseded").is_empty(), "a live request was superseded for its company: {:?}", h.errors());
    assert_eq!(h.device.get("t1", "mode"), Some(json!("cool")));
    assert!(
        h.device.applies().iter().any(|(t, v)| t == "t1" && v.get("temp").is_some() && v.get("mode").is_some()),
        "the live and the replayed request were not taken together, so this test proves nothing: {:?}",
        h.device.applies()
    );
    assert_eq!(h.device.calls_of(Op::Connect).len(), 1, "the link was lost, so this test proves nothing");
    assert!(h.stop().await);
}

/// The read-back, deterministically on the replay path: the device is up and proven before the
/// broker acknowledges the read-back's subscription, so the retained requests arrive for a device
/// that is up and are checked because they were replayed (review MINOR 15).
#[tokio::test(flavor = "multi_thread")]
async fn the_read_back_decides_replays_for_a_device_already_up() {
    let h = Harness::start_unwaited("readback-up", acknowledged(), quick_timing(), |broker, topics, device| {
        broker.hold_subacks();
        broker.retain(&topics.state("t1"), json!({"temp": 20, "power": false}).to_string(), &SendOptions::default());
        let req = SendOptions::default().expiry(3600).user_property("conflict", "DeviceWins");
        broker.retain(&topics.desired("t1", "temp"), "22", &req);
        broker.retain(&topics.desired("t1", "power"), "true", &req);
        device.set("t1", "temp", json!(18));
        device.set("t1", "power", json!(false));
    })
    .await;
    assert!(h.eventually(BOUND, |h| h.device.connected() && !h.device.calls_of(Op::Read).is_empty()).await, "the device never connected");
    h.settle(Duration::from_millis(200)).await;
    h.broker.release_subacks();
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "power") == Some(json!(true))).await,
        "the unchanged property's request was not applied"
    );
    assert!(
        h.eventually(BOUND, |h| h.errors_for("superseded").iter().any(|e| e["property"] == "temp")).await,
        "the replayed request was not decided against the read-back: {:?}",
        h.errors()
    );
    assert_eq!(h.device.get("t1", "temp"), Some(json!(18)));
    assert!(h.stop().await);
}

/// The read-back, deterministically on the held path: the device comes up only after the retained
/// requests were taken, so they are held as well as replayed (review MINOR 15).
#[tokio::test(flavor = "multi_thread")]
async fn the_read_back_decides_replays_held_for_a_device_down() {
    let h = Harness::start_with("readback-down", acknowledged(), quick_timing(), |broker, topics, device| {
        broker.retain(&topics.state("t1"), json!({"temp": 20, "power": false}).to_string(), &SendOptions::default());
        let req = SendOptions::default().expiry(3600).user_property("conflict", "DeviceWins");
        broker.retain(&topics.desired("t1", "temp"), "22", &req);
        broker.retain(&topics.desired("t1", "power"), "true", &req);
        device.set("t1", "temp", json!(18));
        device.set("t1", "power", json!(false));
        device.set_up(false);
    })
    .await;
    assert!(h.broker.wait_for_subscription(&h.topics.desired_property_filter(), BOUND).await);
    h.settle(Duration::from_millis(300)).await;
    h.device.set_up(true);
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "power") == Some(json!(true))).await,
        "the unchanged property's request was not applied"
    );
    assert!(
        h.eventually(BOUND, |h| h.errors_for("superseded").iter().any(|e| e["property"] == "temp")).await,
        "the held replay was not decided against the read-back: {:?}",
        h.errors()
    );
    assert_eq!(h.device.get("t1", "temp"), Some(json!(18)));
    assert!(h.stop().await);
}

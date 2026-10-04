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
    assert_eq!(h.device.applies(), vec![("t1".to_owned(), values(&[("power", json!(true))]))]);
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

//! Commands, replies and the other lanes (Bridge Runtime spec §4.2, §4.4, §5): a command's answer
//! on its response topic with its correlation data; the standard `refresh` and `assume`; read
//! commands; `Event`; assumed feedback, from the driver and from a target's `Config`.

use std::time::Duration;

use bytes::Bytes;
use mqtt_bridge_conformance::*;
use mqtt_bridge_kit::{Feedback, PropertyMap, Writes};
use serde_json::{json, Value};

fn up_at_20(_: &FakeBroker, _: &mqtt_bridge_kit::topics::Topics, device: &Device) {
    device.set("t1", "temp", json!(20));
    device.set("t1", "power", json!(false));
}

async fn connected(h: &Harness) -> bool {
    h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await
}

fn json_of(r: &Received) -> Value {
    serde_json::from_slice(&r.payload).expect("a JSON payload")
}

/// A command's answer goes to its MQTT v5 response topic, carrying its correlation data, shaped as
/// an `Event` (`"kind":"reply"`); so does a refusal's.
#[tokio::test(flavor = "multi_thread")]
async fn a_commands_answer_goes_to_its_response_topic_with_its_correlation_data() {
    let h = Harness::start_with("reply", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    let replies = "Store/replies/one";
    assert!(h.command(json!({"target": "t1", "command": "learn", "key": "Netflix"}), Some((replies, b"c-42"))));
    assert!(h.eventually(BOUND, |h| !h.published(replies).is_empty()).await, "no reply on the response topic");
    let reply = &h.published(replies)[0];
    assert_eq!(reply.correlation_data(), Some(&Bytes::from_static(b"c-42")), "the reply lost the correlation data");
    assert_eq!(json_of(reply), json!({"kind": "reply", "command": "learn", "target": "t1", "value": {"executed": "learn"}}));
    let executed = h.device.calls_of(Op::Execute);
    assert_eq!(executed.len(), 1);
    assert_eq!(executed[0].values, Some(json!({"key": "Netflix"})), "the command's parameters did not reach the driver");

    // A refusal is answered too.
    h.device.set_up(false);
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await);
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"c-43"))));
    assert!(h.eventually(BOUND, |h| h.published(replies).len() == 2).await, "a refused command was not answered");
    let refused = &h.published(replies)[1];
    assert_eq!(refused.correlation_data(), Some(&Bytes::from_static(b"c-43")));
    assert_eq!(json_of(refused)["reason"], "refused");
    assert!(h.stop().await);
}

/// `refresh` reads the device and publishes what it found; `assume` sets the assumed state without
/// calling the driver.
#[tokio::test(flavor = "multi_thread")]
async fn refresh_reads_the_device_and_assume_sets_the_assumed_state_without_it() {
    let h = Harness::start_with("standard", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.set("t1", "temp", json!(25));
    let reads = h.device.calls_of(Op::Read).len();
    assert!(h.command(json!({"target": "t1", "command": "refresh"}), Some(("Store/r", b"1"))));
    assert!(h.eventually(BOUND, |h| h.state("t1").is_some_and(|s| s["temp"] == 25)).await, "refresh did not publish what it read");
    assert_eq!(h.device.calls_of(Op::Read).len(), reads + 1);
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "refresh was not answered");

    let calls = h.device.calls().len();
    assert!(h.command(json!({"target": "t1", "command": "assume", "power": true}), None));
    assert!(
        h.eventually(BOUND, |h| h.state("t1") == Some(json!({"temp": 25, "power": true, "assumed": true}))).await,
        "assume did not set the assumed state: {:?}",
        h.state("t1")
    );
    assert_eq!(h.device.calls().len(), calls, "assume called the driver");
    assert!(h.stop().await);
}

/// A read-class command is answered while the device is up and dropped while it is down.
#[tokio::test(flavor = "multi_thread")]
async fn a_read_command_is_answered_while_up_and_dropped_while_down() {
    let h = Harness::start_with("read", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    assert!(h.command(json!({"target": "t1", "command": "status"}), Some(("Store/r", b"1"))));
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "the read command was not answered");
    h.device.set_up(false);
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await);
    let executed = h.device.calls_of(Op::Execute).len();
    assert!(h.command(json!({"target": "t1", "command": "status"}), Some(("Store/r", b"2"))));
    assert!(h.eventually(BOUND, |h| h.published("Store/r").len() == 2).await, "the dropped read was not answered");
    assert_eq!(json_of(&h.published("Store/r")[1])["reason"], "refused");
    h.device.set_up(true);
    assert!(connected(&h).await);
    h.settle(Duration::from_millis(400)).await;
    assert_eq!(h.device.calls_of(Op::Execute).len(), executed, "a read dropped while down ran later");
    assert!(h.errors().is_empty(), "a dropped read was reported on Error: {:?}", h.errors());
    assert!(h.stop().await);
}

/// A driver's event reaches the `Event` lane.
#[tokio::test(flavor = "multi_thread")]
async fn a_drivers_event_reaches_the_event_lane() {
    let h = Harness::start_with("event", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    assert!(h.device.event(json!({"kind": "code", "target": "t1", "code": "0x20DF"})));
    assert!(h.eventually(BOUND, |h| !h.published(&h.topics.event()).is_empty()).await, "the event never reached the Event lane");
    assert_eq!(json_of(&h.published(&h.topics.event())[0]), json!({"kind": "code", "target": "t1", "code": "0x20DF"}));
    assert!(h.stop().await);
}

/// With assumed feedback, an applied value is published as the target's state marked `"assumed":
/// true` — unless the driver reported the state itself.
#[tokio::test(flavor = "multi_thread")]
async fn assumed_feedback_publishes_applied_values_as_assumed_state() {
    let h = Harness::start("assumed", info(Writes::Acknowledged, Feedback::Assumed), quick_timing()).await;
    assert!(connected(&h).await);
    assert!(h.desire("tv", "power", json!(true), Req::default()));
    assert!(
        h.eventually(BOUND, |h| h.state("tv") == Some(json!({"power": true, "assumed": true}))).await,
        "the applied value was not published as assumed state: {:?}",
        h.state("tv")
    );
    assert!(h.stop().await);
}

/// A target's `Config` reaches `configure`, and its `feedback` overrides the driver's for that
/// target; a retraction reaches `configure` as none.
#[tokio::test(flavor = "multi_thread")]
async fn a_targets_config_is_configured_and_may_override_the_feedback() {
    let h = Harness::start("config", acknowledged(), quick_timing()).await;
    assert!(connected(&h).await);
    assert!(h.config("ir", json!({"code_set": "lg", "feedback": "assumed"})));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Configure).is_empty()).await, "the config never reached the driver");
    assert_eq!(h.device.calls_of(Op::Configure)[0].values, Some(json!({"code_set": "lg", "feedback": "assumed"})));
    assert!(h.desire("ir", "power", json!(true), Req::default()));
    assert!(h.eventually(BOUND, |h| h.state("ir").is_some_and(|s| s["assumed"] == true)).await, "the config's feedback did not apply");
    assert!(h.send_raw(&h.topics.config("ir"), b""));
    assert!(h.eventually(BOUND, |h| h.device.calls_of(Op::Configure).len() == 2).await);
    assert_eq!(h.device.calls_of(Op::Configure)[1].values, None);
    let reported: PropertyMap = [("power".to_owned(), json!(false))].into_iter().collect();
    assert!(h.device.report("ir", reported));
    assert!(
        h.eventually(BOUND, |h| h.state("ir") == Some(json!({"power": false}))).await,
        "a driver's own report did not replace the assumed state"
    );
    assert!(h.stop().await);
}

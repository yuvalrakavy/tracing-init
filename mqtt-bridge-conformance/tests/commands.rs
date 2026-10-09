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
    // The counts tell the calls apart rather than rely on nothing else calling the driver: a
    // refresh of `t1` reads `t1` alone (a poll — off in `quick_timing` anyway — reads everything).
    let reads_of_t1 = |h: &Harness| h.device.calls_of(Op::Read).iter().filter(|c| c.target.as_deref() == Some("t1")).count();
    let reads = reads_of_t1(&h);
    assert!(h.command(json!({"target": "t1", "command": "refresh"}), Some(("Store/r", b"1"))));
    assert!(h.eventually(BOUND, |h| h.state("t1").is_some_and(|s| s["temp"] == 25)).await, "refresh did not publish what it read");
    assert_eq!(reads_of_t1(&h), reads + 1, "refresh did not read its target once");
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "refresh was not answered");

    let not_a_poll = |h: &Harness| h.device.calls().iter().filter(|c| !(c.op == Op::Read && c.target.is_none())).count();
    let calls = not_a_poll(&h);
    assert!(h.command(json!({"target": "t1", "command": "assume", "power": true}), None));
    assert!(
        h.eventually(BOUND, |h| h.state("t1") == Some(json!({"temp": 25, "power": true, "assumed": true}))).await,
        "assume did not set the assumed state: {:?}",
        h.state("t1")
    );
    assert_eq!(not_a_poll(&h), calls, "assume called the driver");
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
    let dropped = json_of(&h.published("Store/r")[1]);
    assert_eq!(dropped["reason"], "refused");
    // Dropped as it came, not queued and dropped by the next failed connect.
    assert_eq!(dropped["error"], "the device is not connected", "the read was not dropped at once: {dropped}");
    h.device.set_up(true);
    assert!(connected(&h).await);
    h.settle(Duration::from_millis(400)).await;
    assert_eq!(h.device.calls_of(Op::Execute).len(), executed, "a read dropped while down ran later");
    // A failure, so on Error too (review C13).
    assert!(h.errors_for("refused").iter().any(|e| e["command"] == "status"), "a dropped read was not reported on Error: {:?}", h.errors());
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

/// A command whose response topic is one of the bridge's own request lanes — `Desired`, `Command`
/// or `Config` — is refused as malformed: its reply would be read back as a request, and a reply
/// on `Command` carries `command`, so a momentary action ran twice (review MINOR 6).
#[tokio::test(flavor = "multi_thread")]
async fn a_response_topic_on_the_bridges_own_request_lanes_is_refused() {
    let h = Harness::start_with("ownlanes", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    let t = h.topics.clone();
    // Its request lanes; its output lanes but Event (where replies arrive), where a reply would
    // pass for the bridge's own report; and any instance's request lanes under the root, where it
    // would be read as a request (the R1 re-gate's R17).
    let other = |lane: &str| format!("{}/{lane}/another", t.root());
    let reserved = [
        t.command(),
        t.desired("t1", "temp"),
        t.config("t1"),
        t.state("t1"),
        t.error(),
        t.status(),
        t.active(),
        t.version(),
        other("Command"),
        format!("{}/t9/x", other("Desired")),
        format!("{}/t9", other("Config")),
    ];
    for own in &reserved {
        assert!(h.command(json!({"target": "t1", "command": "pulse"}), Some((own, b"c"))));
    }
    assert!(
        h.eventually(BOUND, |h| h.errors_for("malformed").len() == reserved.len()).await,
        "a response topic on the bridge's own lanes was not refused: {:?}",
        h.errors()
    );
    h.settle(Duration::from_millis(600)).await;
    assert!(
        h.device.calls_of(Op::Execute).is_empty(),
        "a command answering on the bridge's own lanes ran: {} times",
        h.device.calls_of(Op::Execute).len()
    );
    assert!(h.stop().await);
}

/// For a target with assumed feedback the current value is the assumed model value: a request
/// replayed after a broker blip, already applied, is satisfied — never sent again, which would
/// toggle an IR device (review MINOR 7: the conflict read found nothing on a device that cannot be
/// read, and the request was applied again after every blip).
#[tokio::test(flavor = "multi_thread")]
async fn a_replayed_request_an_assumed_target_already_has_is_not_applied_again() {
    let h = Harness::start_with("assumed-replay", info(Writes::Acknowledged, Feedback::Assumed), quick_timing(), |_, _, device| {
        // A one-way device: nothing can be read back.
        device.answer(Op::Read, None, Answer::Unsupported);
    })
    .await;
    assert!(connected(&h).await);
    // RequestWins, as an IR device's requests are: an unknown current value would be applied.
    assert!(h.desire("tv", "power", json!(true), Req { expiry: Some(3600), conflict: Some("RequestWins") }));
    assert!(h.eventually(BOUND, |h| h.state("tv") == Some(json!({"power": true, "assumed": true}))).await, "the write was not assumed");
    // A broker blip: the request, not retracted yet, comes back as a replay.
    let connects = h.broker.connections();
    h.broker.drop_connection();
    assert!(h.eventually(BOUND, |h| h.broker.connections() > connects).await);
    h.settle(Duration::from_millis(800)).await;
    assert_eq!(h.device.calls_of(Op::Apply).len(), 1, "a replayed request the assumed target already had was sent again");
    assert!(h.stop().await);
}

/// A config identical to the one `configure` last took on this link is not handed to the driver
/// again — every reconnect replays the retained configs; a changed one is, and so is any after the
/// link was lost, when the device may have lost its setup (review MINOR 5).
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_config_is_not_configured_again_while_the_link_stays_up() {
    let h = Harness::start_with("cfgdedupe", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    let configures = |h: &Harness| h.device.calls_of(Op::Configure).len();
    assert!(h.config("ir", json!({"code_set": "lg"})));
    assert!(h.eventually(BOUND, |h| configures(h) == 1).await, "the config never reached the driver");
    let reconnect = |h: &Harness| {
        let connects = h.broker.connections();
        h.broker.drop_connection();
        connects
    };
    let connects = reconnect(&h);
    assert!(h.eventually(BOUND, |h| h.broker.connections() > connects && h.subscribed_again()).await);
    h.settle(Duration::from_millis(500)).await;
    assert_eq!(configures(&h), 1, "an unchanged config replayed on a reconnect was configured again");
    assert!(h.config("ir", json!({"code_set": "sony"})));
    assert!(h.eventually(BOUND, |h| configures(h) == 2).await, "a changed config was not configured");
    // The link lost and back: the same config, replayed, is the device's setup again.
    h.device.set_up(false);
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await);
    h.device.set_up(true);
    assert!(connected(&h).await);
    let connects = reconnect(&h);
    assert!(h.eventually(BOUND, |h| h.broker.connections() > connects && h.subscribed_again()).await);
    assert!(
        h.eventually(BOUND, |h| configures(h) == 3).await,
        "a config replayed after the link was lost was not configured: {}",
        configures(&h)
    );
    assert!(h.stop().await);
}

/// A command refused as it arrives — malformed, or retained — still answers on its response topic,
/// with its correlation data, as well as on `Error` (review C13).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_refused_on_arrival_answers_on_its_response_topic() {
    let h = Harness::start_with("refused-reply", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    let replies = "Store/replies/refused";
    let options = |c: &[u8]| SendOptions::default().expiry(30).response_topic(replies).correlation_data(c.to_vec());
    assert!(h.broker.publish(&h.topics.command(), "not json", &options(b"m-1")));
    assert!(h.broker.publish(&h.topics.command(), json!({"target": "t1", "command": "pulse"}).to_string(), &options(b"r-1").retained()));
    assert!(
        h.eventually(BOUND, |h| h.published(replies).len() == 2).await,
        "a command refused on arrival was not answered: {:?}",
        h.published(replies)
    );
    for (reply, correlation) in h.published(replies).iter().zip([&b"m-1"[..], &b"r-1"[..]]) {
        assert_eq!(reply.correlation_data().map(|c| &c[..]), Some(correlation), "the reply lost its correlation data");
        assert_eq!(json_of(reply)["reason"], "malformed", "{:?}", json_of(reply));
    }
    assert!(h.eventually(BOUND, |h| h.errors_for("malformed").len() == 2).await, "{:?}", h.errors());
    assert!(h.device.calls_of(Op::Execute).is_empty());
    assert!(h.stop().await);
}

/// A momentary command still waiting at its expiry is dropped — and said: on `Error` (`refused`)
/// and on its response topic (review MINOR 4, C13: it was dropped silently).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_that_expires_while_queued_is_reported_and_answered() {
    // Operations may take 3 s here: the slow command below must not cost the link.
    let timing = mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(3)), ..quick_timing() };
    let h = Harness::start_with("expired-reply", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Delay(Duration::from_millis(1500)), 1);
    assert!(h.command(json!({"target": "t1", "command": "learn"}), None));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await);
    let replies = "Store/replies/expired";
    let options = SendOptions::default().expiry(1).response_topic(replies).correlation_data(b"e-1".to_vec());
    assert!(h.broker.publish(&h.topics.command(), json!({"target": "t1", "command": "pulse"}).to_string(), &options));
    assert!(h.eventually(BOUND, |h| !h.published(replies).is_empty()).await, "a command that expired while queued was not answered");
    let reply = &h.published(replies)[0];
    assert_eq!(reply.correlation_data().map(|c| &c[..]), Some(&b"e-1"[..]));
    assert_eq!(json_of(reply)["reason"], "refused");
    assert!(h.errors_for("refused").iter().any(|e| e["command"] == "pulse"), "the expiry was not on Error: {:?}", h.errors());
    assert_eq!(h.device.calls_of(Op::Execute).len(), 1, "a command past its expiry was executed");
    assert!(h.stop().await);
}

/// A `refresh` the device refuses goes to `Error` as well as to its response topic, as every
/// failure does (review C13).
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_refresh_goes_to_error_too() {
    let h = Harness::start_with("refresh-error", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer(Op::Read, Some("t1"), Answer::Rejected);
    assert!(h.command(json!({"target": "t1", "command": "refresh"}), Some(("Store/r", b"f-1"))));
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "the failed refresh was not answered");
    assert!(
        h.eventually(BOUND, |h| h.errors_for("rejected").iter().any(|e| e["command"] == "refresh")).await,
        "a failed refresh was not on Error: {:?}",
        h.errors()
    );
    assert!(h.stop().await);
}

/// A driver retracts a target gone from the device — its retained `State` cleared with an empty
/// retained payload, and not published again after a reconnect — or one property of it (review
/// MINOR 8: nothing was ever removed, and a unit gone from the CoolMaster's list stayed retained).
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_retracts_a_target_or_a_property() {
    let h = Harness::start_with("retract", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set("t1", "mode", json!("cool"));
        device.set("u2", "power", json!(true));
    })
    .await;
    let t = h.topics.clone();
    assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.state("u2")).is_some() && h.state("t1").is_some()).await);
    assert!(h.device.remove("u2"));
    assert!(h.device.remove_property("t1", "mode"));
    assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.state("u2")).is_none()).await, "the removed target's State is still retained");
    assert!(
        h.eventually(BOUND, |h| h.state("t1") == Some(json!({"temp": 20}))).await,
        "the removed property is still published: {:?}",
        h.state("t1")
    );
    let connects = h.broker.connections();
    h.broker.drop_connection();
    assert!(h.eventually(BOUND, |h| h.broker.connections() > connects && h.subscribed_again()).await);
    h.settle(Duration::from_millis(300)).await;
    assert!(h.broker.retained_on(&t.state("u2")).is_none(), "the removed target came back with the model");
    assert!(h.stop().await);
}

/// A command that names no target reaches `execute` with none — not an empty name a driver must
/// know to read as "none" (review MINOR 13).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_with_no_target_reaches_execute_with_none() {
    let h = Harness::start_with("notarget", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    assert!(h.command(json!({"command": "pulse"}), None));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await, "the command was never executed");
    assert_eq!(h.device.calls_of(Op::Execute)[0].target, None, "a command with no target reached execute with one");
    assert!(h.stop().await);
}

/// A command queued while a poll is under way is not lost when the poll loses the link: it is
/// answered (refused while the device is down) or executed, never dropped (the R1 re-gate's R4: the
/// mailbox was served after the failed poll too, and the command it took was discarded with the
/// poll's error).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_queued_during_a_poll_that_loses_the_link_is_answered() {
    let timing = mqtt_bridge_kit::TimingOverrides { poll: Some(Some(Duration::from_millis(200))), ..quick_timing() };
    let h = Harness::start_with("poll-lost", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    let reads = h.device.calls_of(Op::Read).len();
    h.device.answer_times(Op::Read, None, Answer::Hang, 1);
    assert!(h.eventually(BOUND, |h| h.device.calls_of(Op::Read).len() > reads).await, "no poll began");
    let replies = "Store/replies/poll-lost";
    let options = SendOptions::default().expiry(30).response_topic(replies).correlation_data(b"p-1".to_vec());
    assert!(h.broker.publish(&h.topics.command(), json!({"target": "t1", "command": "pulse"}).to_string(), &options));
    // The poll hangs to its deadline and the link is lost with the command still waiting: it is
    // refused as waiting, before it ran — not taken off the mailbox by the failed poll and cut off
    // (re-gate 4: the wording was checked only if a reply came).
    assert!(
        h.eventually(BOUND, |h| !h.published(replies).is_empty()).await,
        "a command queued during a poll that lost the link was not answered: {:?}",
        h.errors()
    );
    let error = json_of(&h.published(replies)[0])["error"].as_str().unwrap_or_default().to_owned();
    assert!(error.contains("before it ran"), "the failed poll took the command off the mailbox: {error}");
    assert!(h.device.calls_of(Op::Execute).is_empty(), "a command waiting when the link was lost was executed");
    assert!(h.stop().await);
}

/// A read-class command still waiting at its expiry is not executed: it is refused, on `Error` and
/// on its response topic — every caller of a coalesced read with its own deadline (the R1
/// re-gate's R9: reads stored no deadline, and ran whenever their turn came).
#[tokio::test(flavor = "multi_thread")]
async fn a_read_command_that_expires_while_queued_is_refused_not_executed() {
    let timing = mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(3)), ..quick_timing() };
    let h = Harness::start_with("expired-read", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Delay(Duration::from_millis(1500)), 1);
    assert!(h.command(json!({"target": "t1", "command": "learn"}), None));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await);
    let replies = "Store/replies/expired-read";
    let options = SendOptions::default().expiry(1).response_topic(replies).correlation_data(b"s-1".to_vec());
    assert!(h.broker.publish(&h.topics.command(), json!({"target": "t1", "command": "status"}).to_string(), &options));
    assert!(h.eventually(BOUND, |h| !h.published(replies).is_empty()).await, "the read was never answered");
    h.settle(Duration::from_millis(300)).await;
    let statuses = h.device.calls_of(Op::Execute).iter().filter(|c| c.command.as_deref() == Some("status")).count();
    assert_eq!(statuses, 0, "a read-class command past its expiry was executed");
    assert_eq!(json_of(&h.published(replies)[0])["reason"], "refused");
    assert!(h.errors_for("refused").iter().any(|e| e["command"] == "status"), "the expiry was not on Error: {:?}", h.errors());
    assert!(h.stop().await);
}

/// An `assume` naming a property that cannot stand as a topic segment is the Store's malformed
/// input: refused as `malformed` and answered — not a driver's contract violation, and not
/// answered `done` (Claude's re-gate minor 6).
#[tokio::test(flavor = "multi_thread")]
async fn an_assume_with_a_bad_property_name_is_malformed() {
    let h = Harness::start_with("assume-bad-name", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    assert!(h.command(json!({"target": "t1", "command": "assume", "a/b": 1}), Some(("Store/r", b"a-1"))));
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "the assume was not answered");
    assert_eq!(json_of(&h.published("Store/r")[0])["reason"], "malformed", "a bad assume was not refused as malformed");
    assert!(h.log.of_kind("driver_contract_violation").is_empty(), "the Store's input was taken for a driver bug");
    assert!(h.stop().await);
}

/// A burst of commands refused at arrival — the device down — each with a response topic: every one
/// is answered. The session publishes what is pending between reads, so replies are not displaced
/// past the event cap while it drains a backlog (the re-gate's RG3-3: with the reads polled before
/// the reports, about 64 of 200 were answered).
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_refused_commands_is_answered_every_one() {
    let h = Harness::start_with("burst", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set_up(false);
    })
    .await;
    let replies = "Store/replies/burst";
    for i in 0..200u32 {
        assert!(h.command(json!({"target": "t1", "command": "pulse"}), Some((replies, &i.to_be_bytes()))));
    }
    h.eventually(Duration::from_secs(5), |h| h.published(replies).len() >= 200).await;
    assert_eq!(h.published(replies).len(), 200, "replies were lost in a burst");
    assert!(h.stop().await);
}

/// A command the worker is executing when the stop comes is answered — refused, on `Error` and on
/// its response topic — as is the one waiting behind it (the re-gate's RG3-2: the stop cancelled it
/// mid-operation, and its reply went with it).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_executing_at_the_stop_is_answered() {
    let timing = mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(3)), ..quick_timing() };
    let mut h = Harness::start_with("stop-in-flight", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Delay(Duration::from_millis(2500)), 1);
    let replies = "Store/replies/stop-in-flight";
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"c-1"))));
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"c-2"))));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await, "never executed");
    assert!(h.stop_bridge().await);
    let answers = answers_by_correlation(&h, replies);
    assert_eq!(answers.len(), 2, "a command in flight at the stop got no reply: {answers:?}");
    for (id, errors) in &answers {
        assert_eq!(errors.len(), 1, "command {id} was answered {} times: {errors:?}", errors.len());
    }
    assert!(answers["c-1"][0].contains("cut off"), "the command cut off at the stop was not said cut off: {answers:?}");
    assert!(answers["c-2"][0].contains("before it ran"), "the command waiting at the stop was not said refused before it ran: {answers:?}");
    assert_eq!(h.errors_for("refused").len(), 2, "a command in flight at the stop was not refused on Error: {:?}", h.errors());
}

/// Each correlation id answered on `replies`, with the error of each of its answers (`""`: a
/// success).
fn answers_by_correlation(h: &Harness, replies: &str) -> std::collections::BTreeMap<String, Vec<String>> {
    let mut answers: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    for reply in h.published(replies) {
        let id = reply.correlation_data().map(|c| String::from_utf8_lossy(c).into_owned()).unwrap_or_default();
        let error = json_of(&reply)["error"].as_str().unwrap_or_default().to_owned();
        answers.entry(id).or_default().push(error);
    }
    answers
}

/// A command whose operation blocks its thread past the watchdog's verdict is answered once: the
/// verdict says it was cut off, and the worker, unblocked later, does not answer it again (re-gate 4
/// F3, its RG4-3: two contradictory answers to one command).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_cut_off_by_the_watchdog_is_answered_once() {
    let h = Harness::start_with("watchdog-in-flight", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Block(Duration::from_millis(3200)), 1);
    let replies = "Store/replies/watchdog-in-flight";
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"w-1"))));
    assert!(h.eventually(BOUND, |h| !h.published(replies).is_empty()).await, "the command the watchdog cut off was not answered");
    // The blocked call returns at 3.2 s; its result must not be a second answer.
    h.settle(Duration::from_secs(2)).await;
    let answers = answers_by_correlation(&h, replies);
    assert_eq!(answers.get("w-1").map(Vec::len), Some(1), "one command answered more than once: {answers:?}");
    assert!(answers["w-1"][0].contains("cut off"), "the command the watchdog cut off was not said cut off: {answers:?}");
    assert!(h.stop().await);
}

/// A device that reports without pause — a fade, a meter — to a broker slow but steady to
/// acknowledge keeps a batch of reports ready at every pass of the session; a command arriving
/// meanwhile is still read and answered: one ready incoming event goes before the next batch
/// (Codex final gate 1: the reports went first every time, and the command waited for good).
#[tokio::test(flavor = "multi_thread")]
async fn a_command_is_answered_under_a_steady_stream_of_reports() {
    let h = Harness::start_with("report-stream", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    // Each ack 200 ms after its publish: the in-flight window fills, and a batch of 50 states takes
    // far longer to publish than the reports take to come.
    h.broker.delay_acks(Duration::from_millis(200));
    let reporting = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let reporter = {
        let (device, reporting) = (h.device.clone(), reporting.clone());
        std::thread::spawn(move || {
            let mut n = 0u64;
            while reporting.load(std::sync::atomic::Ordering::SeqCst) {
                for t in 0..50 {
                    let values: PropertyMap = [("n".to_owned(), json!(n))].into_iter().collect();
                    device.report(&format!("m{t:03}"), values);
                }
                n += 1;
                std::thread::sleep(Duration::from_millis(10));
            }
        })
    };
    h.settle(Duration::from_secs(1)).await;
    let replies = "Store/replies/report-stream";
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"r-1"))));
    let answered = h.eventually(Duration::from_secs(15), |h| !h.published(replies).is_empty()).await;
    reporting.store(false, std::sync::atomic::Ordering::SeqCst);
    let _ = reporter.join();
    h.broker.delay_acks(Duration::ZERO);
    assert!(answered, "a command under a steady stream of reports was never read: the reports went first every time");
    assert!(h.stop().await);
}

/// A stop with the mailbox full — one command executing and 64 waiting, each with a response
/// topic — answers every one, once: the stop's refusals are published directly, not through the
/// reports' capped queues (Codex final gate 2: 65 refusals into 64-entry queues dropped the
/// executing command's answer).
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_with_a_full_mailbox_answers_every_command() {
    let timing = mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(3)), ..quick_timing() };
    let mut h = Harness::start_with("stop-full", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Delay(Duration::from_millis(2500)), 1);
    let replies = "Store/replies/stop-full";
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"c-0"))));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await, "never executed");
    for i in 1..=64 {
        let id = format!("c-{i}");
        assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, id.as_bytes()))));
    }
    // Every one of them waits in the mailbox before the stop.
    h.settle(Duration::from_millis(500)).await;
    assert!(h.stop_bridge().await);
    let answers = answers_by_correlation(&h, replies);
    let missing: Vec<String> = (0..=64).map(|i| format!("c-{i}")).filter(|id| !answers.contains_key(id)).collect();
    assert!(missing.is_empty(), "a stop with a full mailbox left commands unanswered: {missing:?}");
    for (id, errors) in &answers {
        assert_eq!(errors.len(), 1, "command {id} was answered {} times", errors.len());
    }
    assert!(answers["c-0"][0].contains("cut off"), "the executing command was not said cut off: {:?}", answers["c-0"]);
}

/// A watchdog's verdict with the mailbox full — one command blocking the worker's thread past its
/// deadline, 64 waiting, each with a response topic — answers every one, once: its refusals go to
/// the session past the reports' capped queues (second final gate 1: 65 refusals into 64-entry
/// queues dropped the executing command's answer).
#[tokio::test(flavor = "multi_thread")]
async fn a_watchdog_verdict_with_a_full_mailbox_answers_every_command() {
    let h = Harness::start_with("watchdog-full", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Block(Duration::from_millis(4000)), 1);
    let replies = "Store/replies/watchdog-full";
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"c-0"))));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await, "never executed");
    for i in 1..=64 {
        let id = format!("c-{i}");
        assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, id.as_bytes()))));
    }
    // The verdict comes at the deadline plus the grace (about 2.5 s); the blocked call returns at 4 s.
    assert!(
        h.eventually(Duration::from_secs(8), |h| answers_by_correlation(h, replies).len() == 65).await,
        "a watchdog verdict with a full mailbox left commands unanswered: {:?}",
        (0..=64).map(|i| format!("c-{i}")).filter(|id| !answers_by_correlation(&h, replies).contains_key(id)).collect::<Vec<_>>()
    );
    h.settle(Duration::from_secs(2)).await;
    let answers = answers_by_correlation(&h, replies);
    for (id, errors) in &answers {
        assert_eq!(errors.len(), 1, "command {id} was answered {} times", errors.len());
    }
    assert!(answers["c-0"][0].contains("cut off"), "the executing command was not said cut off: {:?}", answers["c-0"]);
    assert!(h.stop().await);
}

/// A link lost with the mailbox full — one command executing, 64 waiting, and a read three
/// callers asked for — answers every caller, once: no refusal goes through a capped queue (the
/// second final gate's item 1, more broadly).
#[tokio::test(flavor = "multi_thread")]
async fn a_link_loss_with_a_full_mailbox_answers_every_caller() {
    let timing = mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(3)), ..quick_timing() };
    let h = Harness::start_with("loss-full", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Execute, None, Answer::Delay(Duration::from_millis(2500)), 1);
    let replies = "Store/replies/loss-full";
    assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, b"c-0"))));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Execute).is_empty()).await, "never executed");
    let mut ids = vec!["c-0".to_owned()];
    for i in 1..=64 {
        let id = format!("c-{i}");
        assert!(h.command(json!({"target": "t1", "command": "learn"}), Some((replies, id.as_bytes()))));
        ids.push(id);
    }
    for i in 1..=3 {
        let id = format!("s-{i}");
        assert!(h.command(json!({"target": "t1", "command": "status"}), Some((replies, id.as_bytes()))));
        ids.push(id);
    }
    h.settle(Duration::from_millis(500)).await;
    h.device.set_up(false);
    assert!(
        h.eventually(Duration::from_secs(8), |h| answers_by_correlation(h, replies).len() == ids.len()).await,
        "a link loss with a full mailbox left callers unanswered: {:?}",
        ids.iter().filter(|id| !answers_by_correlation(&h, replies).contains_key(*id)).collect::<Vec<_>>()
    );
    for (id, errors) in &answers_by_correlation(&h, replies) {
        assert_eq!(errors.len(), 1, "caller {id} was answered {} times", errors.len());
    }
    assert!(h.stop().await);
}

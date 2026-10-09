//! An instance's lifecycle on the multi-device host (multi-device spec §7, scenarios 3 and 4b): a
//! Config change, a failing new factory, disable, delete; and the reconciliation after a
//! reconnect. Each scenario has an outer deadline and a progress assertion.

use std::time::Duration;

use mqtt_bridge_conformance::host::*;
use mqtt_bridge_conformance::*;
use serde_json::json;

/// Each scenario's outer bound.
const DEADLINE: Duration = Duration::from_secs(60);

/// A host serving `instance`, its device connected.
async fn serving(name: &str, instance: &str, fleet: Fleet) -> HostHarness {
    serving_tuned(name, instance, fleet, |host| host).await
}

/// The same, with the host tuned.
async fn serving_tuned(
    name: &str,
    instance: &str,
    fleet: Fleet,
    tune: impl FnOnce(mqtt_bridge_kit::Host) -> mqtt_bridge_kit::Host,
) -> HostHarness {
    let broker = std::sync::Arc::new(FakeBroker::start_multi().await);
    broker.cap_connections(80);
    let h = HostHarness::join(broker, &instance_name(name), fleet, tune).await;
    assert!(h.configure(instance, json!({"address": "a"})), "the Config reached no subscription");
    assert!(
        h.eventually(BOUND, |h| h.status(instance).as_deref() == Some("connected")).await,
        "{instance} never connected: {:?}",
        h.statuses(instance)
    );
    h
}

/// Whether `filter` was subscribed after `unsubscribed` was unsubscribed, in the broker's events.
fn subscribed_since(h: &HostHarness, filter: &str, unsubscribed: &str) -> bool {
    let labels = h.labels();
    let Some(unsub) = labels.iter().position(|l| *l == format!("unsub {unsubscribed}")) else {
        return false;
    };
    labels[unsub..].iter().any(|l| *l == format!("sub {filter}"))
}

/// Scenario 3, a Config change: admission closes first (a late command is refused with its reply,
/// a late property Desired reaches neither device), the worker stops (R1's stop: the command it
/// was running answered "cut off"), its output retires through the rotation, and the successor
/// bootstraps only after the UNSUBACKs — with the new Config, and the late Desired from its own
/// replay.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_change_closes_admission_stops_retires_and_unsubscribes_before_the_successor() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| {
            d.set("t1", "power", json!(false));
            // The old device's stop takes a while: its disconnect blocks the worker for 800 ms.
            d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(800)));
            // And a command it runs takes a while too.
            d.answer(Op::Execute, None, Answer::Delay(Duration::from_secs(5)));
        });
        // Operations bounded at 10 s: the command the old device runs outlasts the change.
        let h = serving_tuned("change", &lamp, fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(10)), ..quick_timing() })
        })
        .await;
        let t = h.topics(&lamp);
        let old = h.fleet.device(&lamp).expect("built");
        // A command the old device is running when the change commits.
        let running_reply = "Store/replies/change-running";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((running_reply, b"r"))));
        assert!(h.eventually(BOUND, |_| !old.calls_of(Op::Execute).is_empty()).await, "the old device never ran the command");
        h.broker.hold_subacks();
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(BOUND, |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the old device was never stopped");
        // Admission is closed: these reach neither device.
        let late_reply = "Store/replies/change-late";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((late_reply, b"l"))));
        assert!(h.desire(&lamp, "t1", "power", json!(true), Req::default()));
        assert!(
            h.eventually(BOUND, |h| h.replies(late_reply).iter().any(|r| r["reason"] == "refused")).await,
            "a command delivered while the device stopped was not refused with its reply: {:?}",
            h.replies(late_reply)
        );
        assert!(
            h.eventually(BOUND, |h| h.replies(running_reply).iter().any(|r| r["error"].as_str().is_some_and(|e| e.contains("cut off"))))
                .await,
            "the command the old device was running was not answered as cut off: {:?}",
            h.replies(running_reply)
        );
        // UNSUBACKs held: the successor has not bootstrapped.
        assert!(
            h.eventually(BOUND, |h| h.labels().iter().any(|l| *l == format!("unsub {}", t.command()))).await,
            "the old device's subscriptions were never unsubscribed: {:#?}",
            h.labels()
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !subscribed_since(&h, &t.state_target_filter(), &t.command()),
            "the successor bootstrapped before the UNSUBACKs: {:#?}",
            h.labels()
        );
        assert_eq!(old.calls_of(Op::Apply).len(), 0, "the old device took a request after admission closed");
        assert!(old.calls_of(Op::Execute).len() == 1, "the old device took the late command");
        h.broker.release_subacks();
        assert!(
            h.eventually(BOUND, |h| h.fleet.devices(&lamp).len() == 2 && h.status(&lamp).as_deref() == Some("connected")).await,
            "the successor never connected: {:?}",
            h.statuses(&lamp)
        );
        assert_eq!(h.fleet.configs(&lamp).last().and_then(|c| c.get("address").cloned()), Some(json!("b")));
        let new = h.fleet.device(&lamp).expect("the successor");
        assert!(
            h.eventually(BOUND, |_| new.get("t1", "power") == Some(json!(true))).await,
            "the late Desired did not reach the successor from its replay"
        );
        assert!(new.calls_of(Op::Execute).is_empty(), "the successor ran the refused command");
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 3, a failing new factory: the current device keeps serving, unchanged — one
/// `Error/{instance}` without a target, WARN `device_config_rejected`, and nothing unsubscribed.
#[tokio::test(flavor = "multi_thread")]
async fn a_failing_new_factory_leaves_the_current_device_serving() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.script(&lamp, &[Build::Ok, Build::Unusable]);
        let h = serving("failing-change", &lamp, fleet).await;
        let old = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(
            h.eventually(BOUND, |h| h.errors_for(&lamp, "unusable").iter().any(|e| e.get("target").is_none())).await,
            "the factory's failure was not one instance-level Error: {:?}",
            h.errors(&lamp)
        );
        assert_eq!(h.log.of_kind("device_config_rejected").len(), 1, "{:#?}", h.log.records());
        let reply = "Store/replies/failing-change";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(
            h.eventually(BOUND, |h| h.replies(reply).iter().any(|r| r["value"] == json!({"executed": "learn"}))).await,
            "the current device stopped serving after a failed change: {:?}",
            h.replies(reply)
        );
        assert!(old.calls_of(Op::Disconnect).is_empty(), "the current device was stopped");
        assert!(!h.labels().iter().any(|l| l.starts_with("unsub ")), "something was unsubscribed: {:#?}", h.labels());
        assert_eq!(h.status(&lamp).as_deref(), Some("connected"));
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 3, delete: the device stops, and the bridge clears only the topics whose current
/// retained value carries its own tag — another process's State on the instance stays.
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_clears_only_the_topics_this_process_tagged() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| {
            d.set("t1", "power", json!(true));
            d.set("t2", "power", json!(false));
        });
        let h = HostHarness::start("delete", fleet).await;
        let t = h.topics(&lamp);
        // Another process's State, on a target this one never reports.
        h.broker.retain(
            &t.state("t3"),
            json!({"power": true}).to_string(),
            &SendOptions::default().retained().user_property("via", "elsewhere"),
        );
        assert!(h.configure(&lamp, json!({})));
        assert!(
            h.eventually(BOUND, |h| h.state(&lamp, "t1").is_some()
                && h.state(&lamp, "t2").is_some()
                && h.status(&lamp).as_deref() == Some("connected"))
                .await,
            "the device's state never reached the broker"
        );
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.delete(&lamp));
        assert!(h.eventually(BOUND, |_| !device.calls_of(Op::Disconnect).is_empty()).await, "the device was not stopped on delete");
        assert!(
            h.eventually(BOUND, |h| [t.status(), t.state("t1"), t.state("t2")].iter().all(|topic| h.broker.retained_on(topic).is_none()))
                .await,
            "the bridge's own tagged topics were not cleared: {:?}",
            h.broker.retained()
        );
        assert!(h.broker.retained_on(&t.state("t3")).is_some(), "another process's topic was cleared");
        assert!(!h.subscribed(&t.state_target_filter()) && !h.subscribed(&t.command()), "the deleted instance stayed subscribed");
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 3, disable: the worker stops, S2 and S3 go, S1 stays, `Status=disabled` — healed over a
/// foreign write — and re-enable bootstraps again from S2 with a new device.
#[tokio::test(flavor = "multi_thread")]
async fn disable_stops_the_worker_keeps_the_read_back_and_heals_its_status() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.set("t1", "power", json!(true)));
        let h = serving("disable", &lamp, fleet).await;
        let t = h.topics(&lamp);
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "a", "disabled": true})));
        assert!(
            h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("disabled")).await,
            "the disabled instance never said so: {:?}",
            h.statuses(&lamp)
        );
        assert!(!device.calls_of(Op::Disconnect).is_empty(), "the worker was not stopped");
        assert!(
            h.eventually(BOUND, |h| !h.subscribed(&t.command()) && !h.subscribed(&t.config_target_filter())).await,
            "S2 and S3 stayed: {:?}",
            h.broker.subscriptions()
        );
        assert!(h.subscribed(&t.state_target_filter()) && h.subscribed(&t.status()), "S1 went with S2 and S3");
        // A foreign write to its Status is healed: `disabled` again, tagged.
        let before = h.statuses(&lamp).len();
        h.broker.publish(&t.status(), "connected", &SendOptions::default().retained().user_property("via", "elsewhere"));
        assert!(
            h.eventually(BOUND, |h| h.statuses(&lamp).len() > before
                && h.status_via(&lamp) == Some(("disabled".into(), Some(h.bridge.clone()))))
                .await,
            "the foreign write was not healed: {:?}",
            h.status_via(&lamp)
        );
        assert_eq!(h.log.of_kind("slot_reasserted").len(), 1, "{:#?}", h.log.records());
        // Re-enabled: a new device, from a fresh bootstrap of S2 and S3.
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(
            h.eventually(BOUND, |h| h.fleet.devices(&lamp).len() == 2 && h.status(&lamp).as_deref() == Some("connected")).await,
            "the re-enabled instance never connected again: {:?}",
            h.statuses(&lamp)
        );
        assert!(h.subscribed(&t.command()) && h.subscribed(&t.config_target_filter()), "S2 and S3 were not subscribed again");
        assert!(h.stop().await);
    })
    .await
}

/// A host serving two instances, disconnected from the broker; `while_away` runs before the broker
/// takes it back.
async fn reconnected(h: &HostHarness, while_away: impl FnOnce(&HostHarness)) {
    let before = h.broker.connections();
    h.broker.refuse_connections(true);
    h.broker.drop_connection();
    tokio::time::sleep(Duration::from_millis(300)).await;
    while_away(h);
    h.broker.refuse_connections(false);
    assert!(h.eventually(BOUND, |h| h.broker.connections() > before).await, "the host never reconnected");
}

/// Scenario 4b: an instance deleted while the host was disconnected is released after the replay
/// (stopped, its tagged topics cleared); the other is untouched.
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_deleted_while_disconnected_is_released_after_the_replay() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (gone, kept) = (instance_name("Gone"), instance_name("Kept"));
        let h = serving("reconcile-delete", &gone, fleet).await;
        assert!(h.configure(&kept, json!({})));
        assert!(h.eventually(BOUND, |h| h.status(&kept).as_deref() == Some("connected")).await);
        let (gone_device, kept_device) = (h.fleet.device(&gone).expect("built"), h.fleet.device(&kept).expect("built"));
        reconnected(&h, |h| h.broker.retain(&h.config_topic(&gone), "", &SendOptions::default())).await;
        assert!(
            h.eventually(BOUND, |_| !gone_device.calls_of(Op::Disconnect).is_empty()).await,
            "the instance deleted while disconnected was not released"
        );
        let t = h.topics(&gone);
        assert!(
            h.eventually(BOUND, |h| h.broker.retained_on(&t.status()).is_none()).await,
            "the released instance's Status was not cleared"
        );
        assert!(kept_device.calls_of(Op::Disconnect).is_empty(), "the other instance was stopped");
        assert_eq!(h.fleet.builds(&kept), 1, "the other instance was rebuilt");
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 4b: a target Config removed while the host was disconnected is retracted —
/// `configure(target, None)` — after the reconnect.
#[tokio::test(flavor = "multi_thread")]
async fn a_target_config_removed_while_disconnected_is_retracted() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("reconcile-target", fleet).await;
        let t = h.topics(&lamp);
        h.broker.retain(&t.config("t1"), json!({"zone": 2}).to_string(), &SendOptions::default().retained());
        assert!(h.configure(&lamp, json!({})));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await);
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.eventually(BOUND, |_| device.calls_of(Op::Configure).iter().any(|c| c.values.is_some())).await, "never configured");
        reconnected(&h, |h| h.broker.retain(&t.config("t1"), "", &SendOptions::default())).await;
        assert!(
            h.eventually(BOUND, |_| device.calls_of(Op::Configure).iter().any(|c| c.target.as_deref() == Some("t1") && c.values.is_none()))
                .await,
            "the target Config no longer retained was not retracted: {:#?}",
            device.calls_of(Op::Configure)
        );
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 4b: an incomplete replay — the barrier's SUBACK overdue, `Active=true` published on
/// the 5 s fallback — releases nothing; the barrier's SUBACK, when it comes, does.
#[tokio::test(flavor = "multi_thread")]
async fn an_incomplete_replay_releases_nothing() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let gone = instance_name("Gone");
        let h = serving("reconcile-incomplete", &gone, fleet).await;
        let device = h.fleet.device(&gone).expect("built");
        let actives = |h: &HostHarness| h.published(&h.active_topic()).iter().filter(|r| &r.payload[..] == b"true").count();
        let first = actives(&h);
        reconnected(&h, |h| {
            h.broker.retain(&h.config_topic(&gone), "", &SendOptions::default());
            h.broker.hold_subacks_matching(&format!("{ROOT}/_barrier/"));
        })
        .await;
        assert!(
            h.eventually(Duration::from_secs(8), |h| actives(h) > first).await,
            "Active=true never came on the fallback with the barrier's SUBACK held"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(device.calls_of(Op::Disconnect).is_empty(), "an incomplete replay released an instance");
        h.broker.release_subacks();
        assert!(
            h.eventually(BOUND, |_| !device.calls_of(Op::Disconnect).is_empty()).await,
            "the barrier's late SUBACK did not complete the replay and release the deleted instance"
        );
        assert!(h.stop().await);
    })
    .await
}

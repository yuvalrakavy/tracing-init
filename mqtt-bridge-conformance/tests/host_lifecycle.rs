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

/// A device's stop that spans a reconnect: the worker stops on the new connection, and its output —
/// the reply of the command it was cut off from — still retires through the rotation there, then the
/// successor bootstraps (a new connection begins with nothing announced).
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_that_spans_a_reconnect_still_retires_its_output() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| {
            d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1500)));
            d.answer(Op::Execute, None, Answer::Delay(Duration::from_secs(5)));
        });
        let h = serving_tuned("stop-reconnect", &lamp, fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { operation: Some(Duration::from_secs(10)), ..quick_timing() })
        })
        .await;
        let old = h.fleet.device(&lamp).expect("built");
        let reply = "Store/replies/stop-reconnect";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"r"))));
        assert!(h.eventually(BOUND, |_| !old.calls_of(Op::Execute).is_empty()).await, "the old device never ran the command");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(BOUND, |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the old device was never stopped");
        // The connection goes while the old worker's disconnect still blocks.
        let before = h.broker.connections();
        h.broker.refuse_connections(true);
        h.broker.drop_connection();
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.broker.refuse_connections(false);
        assert!(h.eventually(BOUND, |h| h.broker.connections() > before).await, "the host never reconnected");
        assert!(
            h.eventually(BOUND, |h| h.replies(reply).iter().any(|r| r["error"].as_str().is_some_and(|e| e.contains("cut off")))).await,
            "the stopped device's output never retired on the new connection: {:?}",
            h.replies(reply)
        );
        assert!(
            h.eventually(BOUND, |h| h.fleet.devices(&lamp).len() == 2 && h.status(&lamp).as_deref() == Some("connected")).await,
            "the successor never bootstrapped after the stop spanned a reconnect: {:?}",
            h.statuses(&lamp)
        );
        assert!(h.stop().await);
    })
    .await
}

/// Deliveries still on the wire when a stopping device's UNSUBSCRIBEs go out get the terminal
/// policy, and their answers are published though the UNSUBACKs end the device's runtime right
/// behind them (multi-device spec §4.4 steps 1 and 3).
#[tokio::test(flavor = "multi_thread")]
async fn answers_refused_while_unsubscribing_are_published() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = serving("unsubscribing", &lamp, fleet).await;
        let t = h.topics(&lamp);
        h.broker.hold_subacks();
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(
            h.eventually(BOUND, |h| h.labels().iter().any(|l| *l == format!("unsub {}", t.command()))).await,
            "the old device's subscriptions were never unsubscribed: {:#?}",
            h.labels()
        );
        // Commands already on their way — straight to the client — then the UNSUBACKs behind them.
        for n in 0..10 {
            let options =
                SendOptions::default().expiry(30).response_topic(&format!("Store/replies/unsub-{n}")).correlation_data(vec![n as u8]);
            assert!(h.broker.send_with(&t.command(), json!({"command": "learn"}).to_string(), &options));
        }
        h.broker.release_subacks();
        assert!(
            h.eventually(BOUND, |h| (0..10)
                .all(|n| h.replies(&format!("Store/replies/unsub-{n}")).iter().any(|r| r["reason"] == "refused")))
                .await,
            "an answer refused while the device unsubscribed was lost with its runtime: {:?}",
            (0..10).map(|n| h.replies(&format!("Store/replies/unsub-{n}")).len()).collect::<Vec<_>>()
        );
        assert!(h.stop().await);
    })
    .await
}

/// A stopping device's retirement is finite: requests that keep arriving through its
/// still-installed subscriptions get their terminal answers through the session's fair queue, and
/// cannot hold the stop open — the delete goes on to its unsubscriptions and its cleanup (R2 gate,
/// Codex 2: builder gap 17's "minor" was a hang).
#[tokio::test(flavor = "multi_thread")]
async fn a_request_flood_cannot_hold_a_stop_open() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = serving("retire-flood", &lamp, fleet).await;
        let t = h.topics(&lamp);
        // A broker slow but steady: each turn of the rotation takes a while, and requests pile up.
        h.broker.delay_acks(Duration::from_millis(20));
        let (broker, command) = (h.broker.clone(), t.command());
        let flood = tokio::spawn(async move {
            let mut n = 0u32;
            loop {
                for _ in 0..20 {
                    let options =
                        SendOptions::default().expiry(30).response_topic("Store/replies/flood").correlation_data(n.to_be_bytes().to_vec());
                    broker.publish(&command, json!({"command": "learn"}).to_string(), &options);
                    n += 1;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(h.delete(&lamp));
        let unsubscribed = h.eventually(BOUND, |h| h.labels().iter().any(|l| *l == format!("unsub {}", t.command()))).await;
        flood.abort();
        h.broker.delay_acks(Duration::ZERO);
        assert!(unsubscribed, "a request flood held the deleted device's stop open: its subscriptions were never removed");
        assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.status()).is_none()).await, "the deleted instance was never cleaned up");
        assert!(h.stop().await);
    })
    .await
}

/// The labels since the broker's last accepted connection.
fn since_last_connect(h: &HostHarness) -> Vec<String> {
    let labels = h.labels();
    let last = labels.iter().rposition(|l| l == "connect").unwrap_or(0);
    labels[last..].to_vec()
}

/// A disable whose stop spans a reconnect: once disabled, the instance keeps its read-back on the
/// new connection, and heals its `Status=disabled` (multi-device spec §4.4 step 5, §4.5).
#[tokio::test(flavor = "multi_thread")]
async fn a_disable_that_spans_a_reconnect_keeps_its_read_back() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1500))));
        let h = serving("disable-reconnect", &lamp, fleet).await;
        let t = h.topics(&lamp);
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "a", "disabled": true})));
        assert!(h.eventually(BOUND, |_| !device.calls_of(Op::Disconnect).is_empty()).await, "the worker was never stopped");
        let before = h.broker.connections();
        h.broker.refuse_connections(true);
        h.broker.drop_connection();
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.broker.refuse_connections(false);
        assert!(h.eventually(BOUND, |h| h.broker.connections() > before).await, "the host never reconnected");
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("disabled")).await, "{:?}", h.statuses(&lamp));
        assert!(
            h.eventually(BOUND, |h| since_last_connect(h).iter().any(|l| *l == format!("sub {}", t.status()))).await,
            "the disabled instance did not subscribe its read-back on the new connection: {:#?}",
            since_last_connect(&h)
        );
        let marked = h.statuses(&lamp).len();
        h.broker.publish(&t.status(), "connected", &SendOptions::default().retained().user_property("via", "elsewhere"));
        assert!(
            h.eventually(BOUND, |h| h.statuses(&lamp).len() > marked && h.status(&lamp).as_deref() == Some("disabled")).await,
            "the disabled instance did not heal its Status after the reconnect: {:?}",
            h.status_via(&lamp)
        );
        assert!(h.stop().await);
    })
    .await
}

/// A delete whose stop ends while the broker is away: its own tagged topics are still cleared, after
/// the reconnect (multi-device spec §4.5, the bridge cleanup).
#[tokio::test(flavor = "multi_thread")]
async fn a_delete_finished_while_disconnected_is_cleared_after_the_reconnect() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| {
            d.set("t1", "power", json!(true));
            d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1000)));
        });
        let h = serving("delete-offline", &lamp, fleet).await;
        let t = h.topics(&lamp);
        assert!(
            h.eventually(BOUND, |h| h.broker.retained_on(&t.state("t1")).is_some()).await,
            "the device's State never reached the broker"
        );
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.delete(&lamp));
        assert!(h.eventually(BOUND, |_| !device.calls_of(Op::Disconnect).is_empty()).await, "the worker was never stopped");
        // Away until the stop has ended.
        let before = h.broker.connections();
        h.broker.refuse_connections(true);
        h.broker.drop_connection();
        tokio::time::sleep(Duration::from_millis(2000)).await;
        h.broker.refuse_connections(false);
        assert!(h.eventually(BOUND, |h| h.broker.connections() > before).await, "the host never reconnected");
        assert!(
            h.eventually(BOUND, |h| [t.status(), t.state("t1")].iter().all(|topic| h.broker.retained_on(topic).is_none())).await,
            "a delete that ended while disconnected left its tagged topics behind: {:?}",
            h.broker.retained()
        );
        assert!(h.stop().await);
    })
    .await
}

/// A delete whose stop ends while the broker is away re-reads the instance's slot before its
/// deferred cleanup, and clears only what still carries its own tag: a `State` another process
/// wrote meanwhile (the instance's new owner) stays (§4.5; R2 gate, Codex 7).
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_cleanup_clears_only_what_still_carries_its_own_tag() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| {
            d.set("t1", "power", json!(true));
            d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1000)));
        });
        let h = serving("deferred-recheck", &lamp, fleet).await;
        let t = h.topics(&lamp);
        assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.state("t1")).is_some()).await, "the device's State never came");
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.delete(&lamp));
        assert!(h.eventually(BOUND, |_| !device.calls_of(Op::Disconnect).is_empty()).await, "the worker was never stopped");
        // Away until the stop has ended; meanwhile another process — the instance's new owner —
        // writes its State.
        let before = h.broker.connections();
        h.broker.refuse_connections(true);
        h.broker.drop_connection();
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let theirs = json!({"power": false}).to_string();
        h.broker.retain(&t.state("t1"), theirs.clone(), &SendOptions::default().retained().user_property("via", "the-new-owner"));
        h.broker.refuse_connections(false);
        assert!(h.eventually(BOUND, |h| h.broker.connections() > before).await, "the host never reconnected");
        assert!(
            h.eventually(BOUND, |h| h.broker.retained_on(&t.status()).is_none()).await,
            "the deleted instance's own Status was not cleared after the reconnect"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            h.broker.retained_on(&t.state("t1")).as_deref(),
            Some(theirs.as_bytes()),
            "a deferred cleanup cleared a State another process tagged"
        );
        assert!(h.stop().await);
    })
    .await
}

/// An instance deleted while its owning process is down (D4's restart): at its first root replay
/// the restarted process finds the retained `Status` tagged with its own id for an instance it does
/// not own, re-reads that instance's slot and clears what carries its tag — so a re-creation under
/// the same bridge id never reads the old `State` as its "before" (§4.5; R2 gate, Claude 3).
#[tokio::test(flavor = "multi_thread")]
async fn an_instance_deleted_while_its_process_was_down_is_cleaned_at_the_restart() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.set("t1", "power", json!(true)));
        let mut h = HostHarness::start("restart-litter", fleet).await;
        let t = h.topics(&lamp);
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.state("t1")).is_some()).await, "the device's State never came");
        h.abort();
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.broker.retain(&h.config_topic(&lamp), "", &SendOptions::default());
        // The restart: the same bridge id, the same broker.
        let restarted = HostHarness::join(h.broker.clone(), &h.bridge, Fleet::new(acknowledged()), |host| host).await;
        let active = format!("{ROOT}/Active/{}", h.bridge);
        assert!(restarted.eventually(BOUND, |r| r.broker.retained_on(&active).as_deref() == Some(&b"true"[..])).await);
        assert!(
            restarted
                .eventually(BOUND, |r| r.broker.retained_on(&t.state("t1")).is_none() && r.broker.retained_on(&t.status()).is_none())
                .await,
            "a deleted instance's own tagged topics survived the restart: State/t1 {:?}, Status {:?}",
            restarted.broker.retained_on(&t.state("t1")),
            restarted.broker.retained_on(&t.status())
        );
        // Re-created, same bridge id; the new device reports only t2.
        restarted.fleet.prepare(&lamp, |d| d.set("t2", "power", json!(false)));
        assert!(restarted.configure(&lamp, json!({"address": "b"})));
        assert!(restarted.eventually(BOUND, |r| r.status(&lamp).as_deref() == Some("connected")).await);
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(restarted.broker.retained_on(&t.state("t1")).is_none(), "the old State came back");
        assert!(restarted.stop().await);
    })
    .await
}

/// A delete's cleanup is pending until the broker has acknowledged it, not merely until it is
/// handed to rumqttc: a connection lost first — its retractions written but never read — leaves it
/// to the next connection, which clears the instance's own tagged topics (§4.5; R2 gate, Claude
/// minor 2).
#[tokio::test(flavor = "multi_thread")]
async fn a_cleanup_lost_with_its_connection_is_done_on_the_next() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.set("t1", "power", json!(true)));
        let h = serving("cleanup-lost", &lamp, fleet).await;
        let t = h.topics(&lamp);
        assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.state("t1")).is_some()).await, "the device's State never came");
        // The UNSUBACKs held; released with the broker no longer reading: the cleanup that follows
        // them is written to a connection that never reads it.
        h.broker.hold_subacks();
        assert!(h.delete(&lamp));
        assert!(
            h.eventually(BOUND, |h| h.labels().iter().any(|l| *l == format!("unsub {}", t.status()))).await,
            "the deleted instance's read-back was never unsubscribed"
        );
        let before = h.broker.connections();
        h.broker.release_subacks();
        h.broker.stop_reading();
        tokio::time::sleep(Duration::from_millis(500)).await;
        h.broker.drop_connection();
        assert!(h.eventually(BOUND, |h| h.broker.connections() > before).await, "the host never reconnected");
        assert!(
            h.eventually(BOUND, |h| [t.status(), t.state("t1")].iter().all(|topic| h.broker.retained_on(topic).is_none())).await,
            "a cleanup lost with its connection was never done: {:?}",
            h.broker.retained()
        );
        assert!(h.stop().await);
    })
    .await
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
        // Refused by the terminal policy as it arrives — not posted to the old mailbox, whose stop
        // would refuse it too, later, as one waiting.
        assert!(
            h.eventually(BOUND, |h| h
                .replies(late_reply)
                .iter()
                .any(|r| r["reason"] == "refused" && r["error"].as_str().is_some_and(|e| e.contains("being stopped"))))
                .await,
            "a command delivered while the device stopped was not refused with its reply by the terminal policy: {:?}",
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
        // The late property write was ignored, not answered: it replayed to the successor.
        assert!(
            h.errors(&lamp).iter().all(|e| e.get("property").is_none()),
            "the late property Desired was answered, not left to replay: {:?}",
            h.errors(&lamp)
        );
        assert!(h.stop().await);
    })
    .await
}

/// Stopping a device answers what waited for it (multi-device spec §4.4 step 2): a property request
/// held while the device was down is refused when the instance is deleted.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_device_answers_its_held_requests() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.set_up(false));
        let h = HostHarness::start("held-at-stop", fleet).await;
        assert!(h.configure(&lamp, json!({})));
        let t = h.topics(&lamp);
        assert!(h.eventually(BOUND, |h| h.subscribed(&t.command())).await, "the instance's requests were never subscribed");
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("unreachable")).await, "{:?}", h.statuses(&lamp));
        assert!(h.desire(&lamp, "t1", "power", json!(true), Req { expiry: Some(60), conflict: None }));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(h.delete(&lamp));
        assert!(
            h.eventually(BOUND, |h| h.errors_for(&lamp, "refused").iter().any(|e| e["target"] == "t1" && e["property"] == "power")).await,
            "the request held for the stopped device was never answered: {:?}",
            h.errors(&lamp)
        );
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
        // (The first root replay's `Status/+` read is unsubscribed once read: no instance's.)
        let instance_unsub = |l: &String| l.starts_with("unsub ") && !l.ends_with("/Status/+");
        assert!(!h.labels().iter().any(instance_unsub), "something was unsubscribed: {:#?}", h.labels());
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
        // The stop ended on its UNSUBACKs, in this session: not on an overdue grant that ended it
        // (the cleanup would come all the same, after the reconnect).
        assert!(
            h.log.of_kind("subscription_refused").is_empty(),
            "the delete's UNSUBACKs were never matched; the session ended on them: {:#?}",
            h.log.of_kind("subscription_refused")
        );
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
        assert_eq!(
            h.log.of_kind("slot_reasserted").len(),
            1,
            "a slot was reasserted other than over the one foreign write (the owner's own echo?): {:#?}",
            h.log.of_kind("slot_reasserted")
        );
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
        // Not before the fallback: the barrier's SUBACK is awaited 5 s from the Config/+ grant.
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert_eq!(actives(&h), first, "Active=true came before the barrier's SUBACK or the replay's fallback");
        assert!(
            h.eventually(Duration::from_secs(5), |h| actives(h) > first).await,
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

/// A target Config the Store creates live while a reconnect's S2 replay is open (between S2's
/// SUBACK and S3's) is present on this connection, though not replayed: the reconciliation at S3's
/// SUBACK leaves it applied (R2 gate, Codex 5 / Claude 2).
#[tokio::test(flavor = "multi_thread")]
async fn a_target_config_created_live_in_the_setup_window_is_not_retracted() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("setup-window", fleet).await;
        let t = h.topics(&lamp);
        assert!(h.configure(&lamp, json!({})));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await);
        let device = h.fleet.device(&lamp).expect("built");
        // Reconnect, S3's SUBACK held on the new connection.
        reconnected(&h, |h| h.broker.hold_subacks_matching(&format!("{ROOT}/Desired/{lamp}"))).await;
        assert!(
            h.eventually(BOUND, |h| since_last_connect(h).iter().any(|l| *l == format!("sub {}", t.command()))).await,
            "S3 was never asked for on the new connection"
        );
        // The Store creates a target Config now: retained, delivered live (its retain flag cleared).
        assert!(h.broker.publish(&t.config("t9"), json!({"zone": 9}).to_string(), &SendOptions::default().retained()));
        tokio::time::sleep(Duration::from_millis(300)).await;
        h.broker.release_subacks();
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let calls: Vec<_> = device.calls_of(Op::Configure).into_iter().filter(|c| c.target.as_deref() == Some("t9")).collect();
        assert!(h.broker.retained_on(&t.config("t9")).is_some());
        assert!(
            calls.last().is_some_and(|c| c.values.is_some()),
            "a target Config still retained was retracted from the device: {calls:#?}"
        );
        assert!(h.stop().await);
    })
    .await
}

//! A multi-device host's Configs (multi-device spec §3, §4.3): the failed state, the Config
//! equality, unreadable and mismatched Configs, a disabled instance's terminal policy, many
//! instances at once, and the fixed bridge's untagged topics. Each scenario has an outer deadline
//! and a progress assertion.

use std::sync::Arc;
use std::time::Duration;

use mqtt_bridge_conformance::host::*;
use mqtt_bridge_conformance::*;
use serde_json::json;

/// Each scenario's outer bound.
const DEADLINE: Duration = Duration::from_secs(60);

/// A host on a fresh broker, tuned.
async fn tuned(name: &str, fleet: Fleet, tune: impl FnOnce(mqtt_bridge_kit::Host) -> mqtt_bridge_kit::Host) -> HostHarness {
    let broker = Arc::new(FakeBroker::start_multi().await);
    broker.cap_connections(80);
    HostHarness::join(broker, &instance_name(name), fleet, tune).await
}

/// A first construction failure: the instance is failed — `Status=unreachable` (tagged), one
/// `Error/{instance}` naming the reason, no target — and its requests get R1's offline policy with
/// refusal: a command and a target-level write refused, with the command's reply. It is retried at
/// the reconnect pacing, saying nothing more, until a construction succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn a_first_construction_failure_leaves_the_instance_failed_and_retried() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.script(&lamp, &[Build::Unusable, Build::Unusable, Build::Unusable]);
        // Retried every 2 s: the failed state lasts long enough to be looked at.
        let h = tuned("failed", fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { retry: Some(Duration::from_secs(2)), ..quick_timing() })
        })
        .await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(
            h.eventually(BOUND, |h| h.status_via(&lamp) == Some(("unreachable".into(), Some(h.bridge.clone())))).await,
            "a failed instance did not say unreachable: {:?}",
            h.status_via(&lamp)
        );
        assert!(h.eventually(BOUND, |h| !h.errors_for(&lamp, "unusable").is_empty()).await, "no Error said why");
        assert!(h.errors_for(&lamp, "unusable").iter().all(|e| e.get("target").is_none()), "{:?}", h.errors(&lamp));
        let t = h.topics(&lamp);
        assert!(h.eventually(BOUND, |h| h.subscribed(&t.command())).await, "a failed instance's requests were not subscribed");
        let reply = "Store/replies/failed";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(
            h.eventually(BOUND, |h| h.replies(reply).iter().any(|r| r["reason"] == "refused")).await,
            "a command to a failed instance was not refused with its reply: {:?}",
            h.replies(reply)
        );
        assert!(h.broker.publish(&t.desired_target("t1"), json!({"power": true}).to_string(), &SendOptions::default()));
        assert!(
            h.eventually(BOUND, |h| h.errors_for(&lamp, "refused").iter().any(|e| e["target"] == "t1" && e.get("command").is_none())).await,
            "a target-level write to a failed instance was not refused: {:?}",
            h.errors(&lamp)
        );
        // Retried at the pacing: the fourth build succeeds; the retries said nothing more.
        assert!(
            h.eventually(Duration::from_secs(12), |h| h.status(&lamp).as_deref() == Some("connected")).await,
            "the failed instance was never retried into service: builds {}",
            h.fleet.builds(&lamp)
        );
        assert_eq!(h.fleet.builds(&lamp), 4);
        assert_eq!(h.errors_for(&lamp, "unusable").len(), 1, "each retry said its failure again: {:?}", h.errors(&lamp));
        assert!(h.stop().await);
    })
    .await
}

/// A changed Config is never a no-op for a failed instance: it is built at once, not at the next
/// retry.
#[tokio::test(flavor = "multi_thread")]
async fn a_changed_config_of_a_failed_instance_is_built_at_once() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.script(&lamp, &[Build::Malformed]);
        let h = tuned("failed-change", fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { retry: Some(Duration::from_secs(30)), ..quick_timing() })
        })
        .await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(BOUND, |h| !h.errors_for(&lamp, "malformed").is_empty()).await, "the malformed fields were not said");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(
            h.eventually(Duration::from_secs(5), |h| h.status(&lamp).as_deref() == Some("connected")).await,
            "a changed Config of a failed instance waited for the next retry"
        );
        assert_eq!(h.fleet.configs(&lamp).last().and_then(|c| c.get("address").cloned()), Some(json!("b")));
        assert!(h.stop().await);
    })
    .await
}

/// Config equality is on the parsed value: an identical Config — the same fields in another order —
/// is a no-op, building nothing and stopping nothing.
#[tokio::test(flavor = "multi_thread")]
async fn an_identical_config_is_a_no_op() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("identical", fleet).await;
        let bridge = h.bridge.clone();
        assert!(h.configure_raw(&lamp, format!(r#"{{"bridge":"{KIND}","via":"{bridge}","address":"a","port":1}}"#).as_bytes()));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.configure_raw(&lamp, format!(r#"{{"port":1,"address":"a","via":"{bridge}","bridge":"{KIND}"}}"#).as_bytes()));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(h.fleet.builds(&lamp), 1, "an identical Config was built again");
        assert!(h.fleet.device(&lamp).expect("built").calls_of(Op::Disconnect).is_empty(), "an identical Config stopped the device");
        assert!(h.stop().await);
    })
    .await
}

/// A Config whose `via` cannot be read: the owner keeps its last accepted Config and logs one WARN
/// `device_config_invalid` — once for the same payload.
#[tokio::test(flavor = "multi_thread")]
async fn an_unreadable_config_keeps_the_last_accepted_one() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("unreadable", fleet).await;
        assert!(h.configure(&lamp, json!({})));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.configure_raw(&lamp, b"{not json"));
        assert!(h.eventually(BOUND, |h| h.log.of_kind("device_config_invalid").len() == 1).await, "no WARN device_config_invalid");
        assert!(h.configure_raw(&lamp, b"{not json"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.log.of_kind("device_config_invalid").len(), 1, "the same unreadable Config was said twice");
        assert!(h.fleet.device(&lamp).expect("built").calls_of(Op::Disconnect).is_empty(), "an unreadable Config stopped the device");
        let reply = "Store/replies/unreadable";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(h.eventually(BOUND, |h| h.replies(reply).iter().any(|r| r.get("value").is_some())).await, "the device stopped serving");
        assert!(h.stop().await);
    })
    .await
}

/// A Config naming this process with another kind: `Error/{instance}` `{reason: malformed, error:
/// "kind mismatch…"}`, said once, and the instance not served.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_of_another_kind_is_rejected_and_not_served() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("kind", fleet).await;
        let config = json!({"bridge": "otherkind", "via": h.bridge}).to_string();
        assert!(h.configure_raw(&lamp, config.as_bytes()));
        assert!(
            h.eventually(BOUND, |h| h
                .errors_for(&lamp, "malformed")
                .iter()
                .any(|e| e["error"].as_str().is_some_and(|s| s.contains("kind mismatch"))))
                .await,
            "the kind mismatch was not said: {:?}",
            h.errors(&lamp)
        );
        assert!(h.configure_raw(&lamp, config.as_bytes()));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(h.errors(&lamp).len(), 1, "the same mismatch was said twice");
        assert_eq!(h.fleet.builds(&lamp), 0, "a Config of another kind was built");
        assert!(!h.subscribed(&h.topics(&lamp).status()), "a Config of another kind was served");
        assert!(h.stop().await);
    })
    .await
}

/// A disabled instance refuses what still arrives on its requests' lanes — a command with its reply,
/// a target-level write — and ignores a property Desired (it is retained, and replays once the
/// instance is enabled).
#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_instance_refuses_what_still_arrives() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("disabled", fleet).await;
        assert!(h.configure(&lamp, json!({"disabled": true})));
        assert!(h.eventually(BOUND, |h| h.status(&lamp).as_deref() == Some("disabled")).await, "{:?}", h.statuses(&lamp));
        assert_eq!(h.fleet.builds(&lamp), 0, "a disabled instance was built");
        let t = h.topics(&lamp);
        // Straight to the client, whatever it subscribed to: what "still arrives".
        let command = SendOptions::default().response_topic("Store/replies/disabled").correlation_data(b"1".to_vec());
        assert!(h.broker.send_with(&t.command(), json!({"command": "learn"}).to_string(), &command));
        assert!(h.broker.send_with(&t.desired_target("t1"), json!({"power": true}).to_string(), &SendOptions::default()));
        assert!(h.broker.send_with(&t.desired("t1", "level"), "3", &SendOptions::default()));
        assert!(
            h.eventually(BOUND, |h| h.replies("Store/replies/disabled").iter().any(|r| r["reason"] == "refused")).await,
            "a command to a disabled instance was not refused with its reply"
        );
        assert!(
            h.eventually(BOUND, |h| h.errors_for(&lamp, "refused").iter().any(|e| e["target"] == "t1" && e.get("command").is_none())).await,
            "a target-level write to a disabled instance was not refused: {:?}",
            h.errors(&lamp)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(h.errors(&lamp).iter().all(|e| e.get("property").is_none()), "a property Desired was refused: {:?}", h.errors(&lamp));
        assert!(h.stop().await);
    })
    .await
}

/// Many instances acquired at once: every subscription's SUBACK matched to its instance by packet id
/// — rumqttc wraps its packet ids at the broker's receive maximum (10 here), so ids come round
/// again while earlier requests with them wait — and every device served.
#[tokio::test(flavor = "multi_thread")]
async fn many_instances_bootstrap_at_once() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let names: Vec<String> = (0..15).map(|i| instance_name(&format!("Many{i}"))).collect();
        for name in &names {
            fleet.prepare(name, |d| d.set("t1", "power", json!(true)));
        }
        let h = HostHarness::start("many", fleet).await;
        for name in &names {
            assert!(h.configure(name, json!({})));
        }
        assert!(
            h.eventually(BOUND, |h| names.iter().all(|n| h.status(n).as_deref() == Some("connected") && h.state(n, "t1").is_some())).await,
            "not every instance was served: {:?}",
            names.iter().map(|n| (n.clone(), h.status(n))).collect::<Vec<_>>()
        );
        assert!(h.log.of_kind("subscription_refused").is_empty(), "{:#?}", h.log.of_kind("subscription_refused"));
        let actives = h.published(&h.active_topic()).iter().filter(|r| &r.payload[..] == b"true").count();
        assert_eq!(actives, 1, "the session ended and came again: Active=true {actives} times");
        assert!(h.stop().await);
    })
    .await
}

/// A fixed bridge (`Bridge::run`, `Bridge::start`) is the host with one fixed device, untagged:
/// its `Status` and `State` carry no `via` (multi-device spec §3, §4.5).
#[tokio::test(flavor = "multi_thread")]
async fn a_fixed_bridge_is_never_tagged() {
    within(DEADLINE, async {
        let h =
            Harness::start_with("untagged", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "power", json!(true))).await;
        assert!(h.eventually(BOUND, |h| h.state("t1").is_some() && h.status().as_deref() == Some("connected")).await);
        for r in h.published(&h.topics.status()).iter().chain(h.published(&h.topics.state("t1")).iter()) {
            assert_eq!(r.user_property("via"), None, "a fixed bridge's {} was tagged", r.topic);
        }
        assert!(h.stop().await);
    })
    .await
}

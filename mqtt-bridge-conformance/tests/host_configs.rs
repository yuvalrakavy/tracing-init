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
            h.eventually(bound(), |h| h.status_via(&lamp) == Some(("unreachable".into(), Some(h.bridge.clone())))).await,
            "a failed instance did not say unreachable: {:?}",
            h.status_via(&lamp)
        );
        assert!(h.eventually(bound(), |h| !h.errors_for(&lamp, "unusable").is_empty()).await, "no Error said why");
        assert!(h.errors_for(&lamp, "unusable").iter().all(|e| e.get("target").is_none()), "{:?}", h.errors(&lamp));
        let t = h.topics(&lamp);
        assert!(h.eventually(bound(), |h| h.subscribed(&t.command())).await, "a failed instance's requests were not subscribed");
        let reply = "Store/replies/failed";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(
            h.eventually(bound(), |h| h.replies(reply).iter().any(|r| r["reason"] == "refused")).await,
            "a command to a failed instance was not refused with its reply: {:?}",
            h.replies(reply)
        );
        assert!(h.broker.publish(&t.desired_target("t1"), json!({"power": true}).to_string(), &SendOptions::default()));
        assert!(
            h.eventually(bound(), |h| h.errors_for(&lamp, "refused").iter().any(|e| e["target"] == "t1" && e.get("command").is_none()))
                .await,
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
        assert!(h.eventually(bound(), |h| !h.errors_for(&lamp, "malformed").is_empty()).await, "the malformed fields were not said");
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

/// A Config change committed — the old device stopping for its successor — then changed again, the
/// newest one's construction failing: the committed Config is the last accepted one, and stays in
/// force (§3) — its device starts and serves, and only the newest one's `Error` is said; the
/// instance is not failed (R2 gate, Codex 4 / Claude 4; builder gap 12 overturned).
#[tokio::test(flavor = "multi_thread")]
async fn a_change_whose_newest_construction_fails_keeps_the_last_accepted_config_in_force() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        // Builds: the first device, the committed change, the newest — failing.
        fleet.script(&lamp, &[Build::Ok, Build::Ok, Build::Unusable]);
        fleet.prepare(&lamp, |d| d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1000))));
        let h = tuned("change-then-fail", fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { retry: Some(Duration::from_secs(30)), ..quick_timing() })
        })
        .await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        let old = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(bound(), |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the change was never committed");
        assert!(h.configure(&lamp, json!({"address": "c"})));
        assert!(h.eventually(bound(), |h| !h.errors_for(&lamp, "unusable").is_empty()).await, "the newest Config's failure was not said");
        let committed = h.fleet.devices(&lamp).get(1).cloned().expect("the committed Config's device was built");
        assert!(
            h.eventually(bound(), |h| committed.connected() && h.status(&lamp).as_deref() == Some("connected")).await,
            "the last accepted Config was not kept in force when a newer one failed: statuses {:?}, errors {:?}",
            h.statuses(&lamp),
            h.errors(&lamp)
        );
        assert_eq!(h.fleet.configs(&lamp)[1].get("address"), Some(&json!("b")));
        let reply = "Store/replies/change-then-fail";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(
            h.eventually(bound(), |h| h.replies(reply).iter().any(|r| r.get("value").is_some())).await,
            "the last accepted Config's device did not serve: {:?}",
            h.replies(reply)
        );
        assert!(h.stop().await);
    })
    .await
}

/// A disabled instance enabled again whose construction fails is failed (§4.3) — `unreachable`,
/// retried at the pacing until it serves — not left saying `disabled` and never retried (R2 gate,
/// Claude 1).
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_re_enable_is_failed_and_retried() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.script(&lamp, &[Build::Unusable]);
        let h = tuned("re-enable-fails", fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { retry: Some(Duration::from_secs(1)), ..quick_timing() })
        })
        .await;
        assert!(h.configure(&lamp, json!({"address": "a", "disabled": true})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("disabled")).await);
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| !h.errors_for(&lamp, "unusable").is_empty()).await, "no Error for the failed build");
        assert!(
            h.eventually(Duration::from_secs(8), |h| h.fleet.builds(&lamp) >= 2 && h.status(&lamp).as_deref() == Some("connected")).await,
            "the enabled instance whose first build failed was never retried; builds {}, statuses {:?}",
            h.fleet.builds(&lamp),
            h.statuses(&lamp)
        );
        assert!(
            h.statuses(&lamp).iter().any(|s| s == "unreachable"),
            "the failed re-enable never said unreachable: {:?}",
            h.statuses(&lamp)
        );
        assert!(h.stop().await);
    })
    .await
}

/// A failed instance's retry is no service: while the retry's construction runs, a target-level
/// write is still refused, not held for a device that may never come (R2 gate, Codex 10).
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_keeps_the_failed_refusal_policy() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        // The first build fails; the retry blocks its factory 1.5 s (under the 2 s factory bound).
        fleet.script(&lamp, &[Build::Unusable, Build::Block(Duration::from_millis(1500))]);
        let h = tuned("retry-policy", fleet, |host| {
            host.timing(mqtt_bridge_kit::TimingOverrides { retry: Some(Duration::from_secs(1)), ..quick_timing() })
        })
        .await;
        let t = h.topics(&lamp);
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("unreachable")).await, "{:?}", h.statuses(&lamp));
        assert!(h.eventually(bound(), |h| h.subscribed(&t.desired_target_filter())).await);
        assert!(h.eventually(bound(), |h| h.fleet.builds(&lamp) == 2).await, "the failed instance was never retried");
        assert!(h.broker.publish(&t.desired_target("t1"), json!({"power": true}).to_string(), &SendOptions::default()));
        assert!(
            h.eventually(Duration::from_millis(1000), |h| h
                .errors_for(&lamp, "refused")
                .iter()
                .any(|e| e["target"] == "t1" && e.get("command").is_none()))
                .await,
            "a target-level write during a failed instance's retry was not refused: {:?}",
            h.errors(&lamp)
        );
        let device = h.built(&lamp).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(device.applies().is_empty(), "a write refused during the retry was applied: {:?}", device.applies());
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
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.configure_raw(&lamp, format!(r#"{{"port":1,"address":"a","via":"{bridge}","bridge":"{KIND}"}}"#).as_bytes()));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(h.fleet.builds(&lamp), 1, "an identical Config was built again");
        assert!(h.fleet.device(&lamp).expect("built").calls_of(Op::Disconnect).is_empty(), "an identical Config stopped the device");
        assert!(h.stop().await);
    })
    .await
}

/// Config equality is on the parsed, validated value — the owner, `disabled` and the driver fields
/// (§3): the same Config with `"disabled": false` written out is no change, and stops nothing (R2
/// gate, Claude minor 1).
#[tokio::test(flavor = "multi_thread")]
async fn a_config_differing_only_in_a_default_host_field_is_a_no_op() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        let h = HostHarness::start("default-field", fleet).await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.configure(&lamp, json!({"address": "a", "disabled": false})));
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert_eq!(h.fleet.builds(&lamp), 1, "a Config differing only in `disabled: false` was built again");
        assert!(
            h.fleet.device(&lamp).expect("built").calls_of(Op::Disconnect).is_empty(),
            "a Config differing only in `disabled: false` stopped the device"
        );
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
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        assert!(h.configure_raw(&lamp, b"{not json"));
        assert!(h.eventually(bound(), |h| h.log.of_kind("device_config_invalid").len() == 1).await, "no WARN device_config_invalid");
        assert!(h.configure_raw(&lamp, b"{not json"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.log.of_kind("device_config_invalid").len(), 1, "the same unreadable Config was said twice");
        assert!(h.fleet.device(&lamp).expect("built").calls_of(Op::Disconnect).is_empty(), "an unreadable Config stopped the device");
        let reply = "Store/replies/unreadable";
        assert!(h.command(&lamp, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(h.eventually(bound(), |h| h.replies(reply).iter().any(|r| r.get("value").is_some())).await, "the device stopped serving");
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
            h.eventually(bound(), |h| h
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

/// A Config/{i} whose name the Store would never show (a sort prefix here) is not served, whoever
/// wrote it: `Error/{instance}` `{reason: malformed}` naming the rule, said once, nothing built
/// (multi-device spec §11 G1). A plain name beside it is served.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_named_as_the_store_would_never_show_is_rejected_and_not_served() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let bad = format!("_z_{}", instance_name("Lamp"));
        let good = instance_name("Lamp");
        let h = HostHarness::start("wire-id", fleet).await;
        assert!(h.configure(&bad, json!({})));
        assert!(
            h.eventually(bound(), |h| h
                .errors_for(&bad, "malformed")
                .iter()
                .any(|e| e["error"].as_str().is_some_and(|s| s.contains("sort prefix"))))
                .await,
            "the refused name was not said: {:?}",
            h.errors(&bad)
        );
        assert!(h.configure(&good, json!({})));
        assert!(h.eventually(bound(), |h| h.fleet.builds(&good) == 1).await, "a plain name was not served");
        assert!(h.configure(&bad, json!({})));
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(h.errors(&bad).len(), 1, "the same refusal was said twice");
        assert_eq!(h.fleet.builds(&bad), 0, "a name the Store would never show was built");
        assert!(!h.subscribed(&h.topics(&bad).status()), "a name the Store would never show was served");
        assert!(h.stop().await);
    })
    .await
}

/// A served instance whose Config turns to another kind is released without the delete's cleanup:
/// §4.5 clears an owner's tagged topics on a delete or a reconciled absence only, and a kind never
/// changes in place (R2 gate, Codex minor; builder gap 4 overturned).
#[tokio::test(flavor = "multi_thread")]
async fn a_kind_mismatch_releases_without_cleanup() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.set("t1", "power", json!(true)));
        let h = HostHarness::start("kind-release", fleet).await;
        let t = h.topics(&lamp);
        assert!(h.configure(&lamp, json!({})));
        assert!(h.eventually(bound(), |h| h.broker.retained_on(&t.state("t1")).is_some()).await, "the device's State never came");
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.configure_raw(&lamp, json!({"bridge": "otherkind", "via": h.bridge}).to_string().as_bytes()));
        assert!(h.eventually(bound(), |_| !device.calls_of(Op::Disconnect).is_empty()).await, "the mismatched instance was not released");
        assert!(h.eventually(bound(), |h| !h.subscribed(&t.status())).await, "the released instance's read-back stayed subscribed");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(h.broker.retained_on(&t.state("t1")).is_some(), "a kind mismatch retracted the instance's State");
        assert!(h.broker.retained_on(&t.status()).is_some(), "a kind mismatch retracted the instance's Status");
        assert!(h.stop().await);
    })
    .await
}

/// A disable whose driver fields changed is validated first (§3): a served instance whose Config
/// turns to `disabled: true` with invalid driver fields keeps its last accepted Config — still
/// served, not disabled — and the invalid one's `Error` is said (R2 gate, Codex minor).
#[tokio::test(flavor = "multi_thread")]
async fn a_disable_with_invalid_driver_fields_keeps_the_last_accepted_config() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        // Builds: the served device, then the disable's new fields — invalid.
        fleet.script(&lamp, &[Build::Ok, Build::Malformed]);
        let h = HostHarness::start("disable-invalid", fleet).await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        let device = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "not an address", "disabled": true})));
        assert!(
            h.eventually(bound(), |h| !h.errors_for(&lamp, "malformed").is_empty()).await,
            "a disable with invalid driver fields was not refused: statuses {:?}, errors {:?}",
            h.statuses(&lamp),
            h.errors(&lamp)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(device.calls_of(Op::Disconnect).is_empty(), "a disable with invalid driver fields stopped the device");
        assert_eq!(h.status(&lamp).as_deref(), Some("connected"), "{:?}", h.statuses(&lamp));
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
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("disabled")).await, "{:?}", h.statuses(&lamp));
        assert_eq!(h.fleet.builds(&lamp), 0, "a disabled instance was built");
        let t = h.topics(&lamp);
        // Straight to the client, whatever it subscribed to: what "still arrives".
        let command = SendOptions::default().response_topic("Store/replies/disabled").correlation_data(b"1".to_vec());
        assert!(h.broker.send_with(&t.command(), json!({"command": "learn"}).to_string(), &command));
        assert!(h.broker.send_with(&t.desired_target("t1"), json!({"power": true}).to_string(), &SendOptions::default()));
        assert!(h.broker.send_with(&t.desired("t1", "level"), "3", &SendOptions::default()));
        assert!(
            h.eventually(bound(), |h| h.replies("Store/replies/disabled").iter().any(|r| r["reason"] == "refused")).await,
            "a command to a disabled instance was not refused with its reply"
        );
        assert!(
            h.eventually(bound(), |h| h.errors_for(&lamp, "refused").iter().any(|e| e["target"] == "t1" && e.get("command").is_none()))
                .await,
            "a target-level write to a disabled instance was not refused: {:?}",
            h.errors(&lamp)
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(h.errors(&lamp).iter().all(|e| e.get("property").is_none()), "a property Desired was refused: {:?}", h.errors(&lamp));
        assert!(h.stop().await);
    })
    .await
}

/// While a subscription's SUBACK is outstanding, no packet id it holds is sent again (MQTT §2.2.1):
/// rumqttc wraps its ids at the broker's receive maximum (10 here) and checks only its publishes, so
/// a device's State published meanwhile would reuse it — the host hands a SUBSCRIBE or UNSUBSCRIBE
/// over only on a line clear of publishes, and while one is outstanding hands out fewer ids than
/// the space holds (R2 gate, Codex 3).
#[tokio::test(flavor = "multi_thread")]
async fn no_packet_id_is_reused_while_its_acknowledgement_is_outstanding() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (busy, slow) = (instance_name("Abusy"), instance_name("Bslow"));
        fleet.prepare(&busy, |d| d.set("t0", "n", json!(0)));
        let h = HostHarness::start("pkids", fleet).await;
        assert!(h.configure(&busy, json!({})));
        assert!(h.eventually(bound(), |h| h.status(&busy).as_deref() == Some("connected")).await, "the busy instance never connected");
        let device = h.fleet.device(&busy).expect("built");
        // The slow instance's read-back SUBACK is held: its packet id stays in use.
        h.broker.hold_subacks_matching(&format!("{ROOT}/State/{slow}"));
        assert!(h.configure(&slow, json!({})));
        assert!(
            h.eventually(bound(), |h| h.subscribed(&h.topics(&slow).status())).await,
            "the slow instance's read-back was never asked for"
        );
        // Enough State meanwhile for rumqttc's ids to come round more than once.
        for t in 0..30 {
            let mut values = mqtt_bridge_kit::PropertyMap::new();
            values.insert("n".into(), json!(1));
            device.report(&format!("t{t}"), values);
        }
        tokio::time::sleep(Duration::from_millis(800)).await;
        h.broker.release_subacks();
        assert!(
            h.eventually(bound(), |h| h.status(&slow).as_deref() == Some("connected") && h.state(&busy, "t29").is_some()).await,
            "the instances were not served once the SUBACK came: {:?}",
            h.statuses(&slow)
        );
        assert!(
            h.broker.pkid_reuses().is_empty(),
            "a packet id was reused while its acknowledgement was outstanding: {:#?}",
            h.broker.pkid_reuses()
        );
        assert!(h.stop().await);
    })
    .await
}

/// A reconnect: the busy instance's read-back is granted, and its whole retained model goes out
/// again — through the publication's budget, as every publish does — while the slow instance's
/// read-back SUBACK is held: no publish takes the held request's packet id (MQTT §2.2.1; the R2
/// confirming reviews' packet-id finding — the model went out by direct publishes, outside the
/// budget).
#[tokio::test(flavor = "multi_thread")]
async fn a_read_grants_republish_stays_within_the_packet_id_budget() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (busy, slow) = (instance_name("Abusy"), instance_name("Bslow"));
        fleet.prepare(&busy, |d| d.set("t0", "n", json!(0)));
        let h = HostHarness::start("pkids-rep", fleet).await;
        assert!(h.configure(&busy, json!({})));
        assert!(h.configure(&slow, json!({})));
        assert!(h.eventually(bound(), |h| h.status(&busy).as_deref() == Some("connected")).await);
        assert!(h.eventually(bound(), |h| h.status(&slow).as_deref() == Some("connected")).await);
        let device = h.fleet.device(&busy).expect("built");
        for t in 0..30 {
            let mut values = mqtt_bridge_kit::PropertyMap::new();
            values.insert("n".into(), json!(1));
            device.report(&format!("t{t}"), values);
        }
        assert!(h.eventually(bound(), |h| h.state(&busy, "t29").is_some()).await);
        h.broker.hold_subacks_matching(&format!("{ROOT}/State/{slow}"));
        let before = h.broker.accepted();
        let mark = h.published(&h.topics(&busy).state("t29")).len();
        h.broker.drop_connection();
        assert!(h.eventually(bound(), |h| h.broker.accepted() > before).await, "no reconnect");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let reuses = h.broker.pkid_reuses();
        h.broker.release_subacks();
        assert!(reuses.is_empty(), "a packet id was reused while its acknowledgement was outstanding: {reuses:#?}");
        assert!(
            h.eventually(bound(), |h| h.published(&h.topics(&busy).state("t29")).len() > mark
                && h.status(&slow).as_deref() == Some("connected"))
                .await,
            "the model was not republished once the SUBACK came"
        );
        assert!(h.stop().await);
    })
    .await
}

/// The shutdown's flush with a request's SUBACK outstanding: it publishes no more than the ids left
/// before that request's packet id would come round, and once its ack wait has passed it publishes
/// nothing more — no `Active=false` and no DISCONNECT, so the last will says `Active=false` — rather
/// than everything at once, on ids still in use (MQTT §2.2.1; the R2 confirming reviews: the flush
/// published unbudgeted after its ack wait).
#[tokio::test(flavor = "multi_thread")]
async fn the_shutdown_flush_stays_within_the_packet_id_budget() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (busy, slow) = (instance_name("Abusy"), instance_name("Bslow"));
        fleet.prepare(&busy, |d| d.set("t0", "n", json!(0)));
        let mut h = HostHarness::start("pkids-flush", fleet).await;
        assert!(h.configure(&busy, json!({})));
        assert!(h.eventually(bound(), |h| h.status(&busy).as_deref() == Some("connected")).await, "the busy instance never connected");
        let device = h.fleet.device(&busy).expect("built");
        // The slow instance's read-back SUBACK is held for good: its packet id stays in use.
        h.broker.hold_subacks_matching(&format!("{ROOT}/State/{slow}"));
        assert!(h.configure(&slow, json!({})));
        assert!(
            h.eventually(bound(), |h| h.subscribed(&h.topics(&slow).status())).await,
            "the slow instance's read-back was never asked for"
        );
        // More State than the ids left: most of it still waits when the stop comes.
        for t in 0..30 {
            let mut values = mqtt_bridge_kit::PropertyMap::new();
            values.insert("n".into(), json!(1));
            device.report(&format!("t{t}"), values);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(h.stop_host().await, "the host did not stop in time");
        assert!(
            h.broker.pkid_reuses().is_empty(),
            "the shutdown's flush reused a packet id while its acknowledgement was outstanding: {:#?}",
            h.broker.pkid_reuses()
        );
        assert!(
            h.eventually(bound(), |h| h.broker.retained_on(&h.active_topic()).as_deref() == Some(&b"false"[..])).await,
            "neither the flush nor the last will said Active=false"
        );
        h.broker.release_subacks();
        h.check_lanes();
    })
    .await
}

/// A Config change committed — the old device stopping for its successor — then a disable whose
/// driver fields are invalid: the disable is validated before it touches the stop or the committed
/// driver (§3), so the committed Config stays in force — its device starts and serves — and only the
/// disable's `Error` is said; the instance is never disabled (the R2 confirming reviews, Codex 2).
#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_disable_during_a_replacement_keeps_the_committed_config() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        // Builds: the first device, the committed change, the disable's fields — invalid.
        fleet.script(&lamp, &[Build::Ok, Build::Ok, Build::Malformed]);
        fleet.prepare(&lamp, |d| d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1000))));
        let h = HostHarness::start("disable-in-change", fleet).await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        let old = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(bound(), |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the change was never committed");
        assert!(h.configure(&lamp, json!({"address": "not an address", "disabled": true})));
        assert!(
            h.eventually(bound(), |h| !h.errors_for(&lamp, "malformed").is_empty()).await,
            "the invalid disable's Error was not said: statuses {:?}, errors {:?}",
            h.statuses(&lamp),
            h.errors(&lamp)
        );
        let committed = h.fleet.devices(&lamp).get(1).cloned().expect("the committed Config's device was built");
        assert!(
            h.eventually(bound(), |h| committed.connected() && h.status(&lamp).as_deref() == Some("connected")).await,
            "the committed Config was not kept in force when an invalid disable came during the replacement: statuses {:?}, errors {:?}",
            h.statuses(&lamp),
            h.errors(&lamp)
        );
        assert!(!h.statuses(&lamp).iter().any(|s| s == "disabled"), "an invalid disable disabled the instance: {:?}", h.statuses(&lamp));
        assert_eq!(h.fleet.configs(&lamp)[1].get("address"), Some(&json!("b")));
        assert!(h.stop().await);
    })
    .await
}

/// The same with valid driver fields: once validated, the disable takes over the stop — the
/// committed driver is dropped unstarted and the instance is disabled.
#[tokio::test(flavor = "multi_thread")]
async fn a_valid_disable_during_a_replacement_disables_the_instance() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let lamp = instance_name("Lamp");
        fleet.prepare(&lamp, |d| d.answer(Op::Disconnect, None, Answer::Block(Duration::from_millis(1000))));
        let h = HostHarness::start("valid-disable-in-change", fleet).await;
        assert!(h.configure(&lamp, json!({"address": "a"})));
        assert!(h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("connected")).await);
        let old = h.fleet.device(&lamp).expect("built");
        assert!(h.configure(&lamp, json!({"address": "b"})));
        assert!(h.eventually(bound(), |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the change was never committed");
        assert!(h.configure(&lamp, json!({"address": "c", "disabled": true})));
        assert!(
            h.eventually(bound(), |h| h.status(&lamp).as_deref() == Some("disabled")).await,
            "the valid disable did not disable the instance: statuses {:?}, errors {:?}",
            h.statuses(&lamp),
            h.errors(&lamp)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
        let committed = h.fleet.devices(&lamp).get(1).cloned().expect("the committed Config's device was built");
        assert!(committed.calls_of(Op::Connect).is_empty(), "the committed driver started though a disable replaced it");
        assert_eq!(h.status(&lamp).as_deref(), Some("disabled"), "{:?}", h.statuses(&lamp));
        // The stop itself ended disabled: no successor bootstrapped (`connecting`) in between.
        let statuses = h.statuses(&lamp);
        let served = statuses.iter().position(|s| s == "connected").expect("the first device served");
        assert!(
            statuses[served..].iter().all(|s| s != "connecting"),
            "the validated disable did not take over the stop — a successor bootstrapped first: {statuses:?}"
        );
        assert!(h.errors(&lamp).is_empty(), "{:?}", h.errors(&lamp));
        assert!(h.stop().await);
    })
    .await
}

/// Many instances acquired at once: every subscription's SUBACK matched to its instance by packet
/// id, and every device served.
#[tokio::test(flavor = "multi_thread")]
async fn many_instances_bootstrap_at_once() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let names: Vec<String> = (0..15).map(|i| instance_name(&format!("Many{i}"))).collect();
        for name in &names {
            fleet.prepare(name, |d| d.set("t1", "power", json!(true)));
        }
        // Every Config in the one replay: every read-back subscribed before any SUBACK is read.
        let broker = Arc::new(FakeBroker::start_multi().await);
        broker.cap_connections(80);
        let bridge = instance_name("many");
        for name in &names {
            let config = json!({"bridge": KIND, "via": bridge}).to_string();
            broker.retain(&format!("{ROOT}/Config/{name}"), config, &SendOptions::default().retained());
        }
        let h = HostHarness::join(broker, &bridge, fleet, |host| host).await;
        assert!(
            h.eventually(bound(), |h| names.iter().all(|n| h.status(n).as_deref() == Some("connected") && h.state(n, "t1").is_some()))
                .await,
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

/// The host's stop stops every device's worker — its driver disconnected — before the flush's last
/// words, within the bounded shutdown.
#[tokio::test(flavor = "multi_thread")]
async fn the_hosts_stop_stops_every_device() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (a, b) = (instance_name("Aone"), instance_name("Btwo"));
        let mut h = HostHarness::start("stop-all", fleet).await;
        assert!(h.configure(&a, json!({})) && h.configure(&b, json!({})));
        assert!(
            h.eventually(bound(), |h| h.status(&a).as_deref() == Some("connected") && h.status(&b).as_deref() == Some("connected")).await,
            "the instances never connected"
        );
        let devices = [h.fleet.device(&a).expect("built"), h.fleet.device(&b).expect("built")];
        assert!(h.stop_host().await, "the host did not stop in time");
        for device in &devices {
            assert!(!device.calls_of(Op::Disconnect).is_empty(), "a device's worker was not stopped by the host's stop");
        }
        assert_eq!(h.broker.retained_on(&h.active_topic()).as_deref(), Some(&b"false"[..]), "the stop did not say Active=false");
        h.check_lanes();
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
        assert!(h.eventually(bound(), |h| h.state("t1").is_some() && h.status().as_deref() == Some("connected")).await);
        for r in h.published(&h.topics.status()).iter().chain(h.published(&h.topics.state("t1")).iter()) {
            assert_eq!(r.user_property("via"), None, "a fixed bridge's {} was tagged", r.topic);
        }
        assert!(h.stop().await);
    })
    .await
}

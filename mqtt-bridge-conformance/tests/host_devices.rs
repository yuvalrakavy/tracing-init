//! The multi-device host's devices (multi-device spec §7, scenarios 1, 2, 4c and 7): devices
//! independent of one another, an instance acquired and bootstrapped from its own replay, the
//! bootstrap's order, and fairness between devices. Each scenario has an outer deadline and a
//! progress assertion.

use std::time::{Duration, Instant};

use mqtt_bridge_conformance::host::*;
use mqtt_bridge_conformance::*;
use serde_json::json;

/// Each scenario's outer bound.
const DEADLINE: Duration = Duration::from_secs(60);

/// Scenario 1. Two devices; one never replies: it goes `unreachable` and retries; the other
/// answers within 1 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_that_never_replies_leaves_the_other_answering() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (salon, bedroom) = (instance_name("Salon"), instance_name("Bedroom"));
        fleet.prepare(&salon, |d| d.set("t1", "power", json!(true)));
        fleet.prepare(&bedroom, |d| {
            d.set("t1", "power", json!(false));
            d.answer(Op::Read, None, Answer::Hang);
        });
        let h = HostHarness::start("one-silent", fleet).await;
        assert!(h.configure(&salon, json!({"address": "10.0.0.1"})), "the Config reached no subscription");
        assert!(h.configure(&bedroom, json!({"address": "10.0.0.2"})), "the Config reached no subscription");
        assert!(
            h.eventually(bound(), |h| h.status(&salon).as_deref() == Some("connected")).await,
            "the answering device never connected: {:?}",
            h.statuses(&salon)
        );
        assert!(
            h.eventually(bound(), |h| h.status(&bedroom).as_deref() == Some("unreachable")).await,
            "the device that never replies was not said unreachable: {:?}",
            h.statuses(&bedroom)
        );
        let reply = "Store/replies/one-silent";
        let sent = Instant::now();
        assert!(h.command(&salon, json!({"command": "learn", "target": "t1"}), Some((reply, b"1"))), "the command reached nothing");
        assert!(
            h.eventually(Duration::from_secs(1), |h| !h.replies(reply).is_empty()).await,
            "the other device did not answer within 1 s while one device never replied"
        );
        assert_eq!(h.replies(reply)[0]["value"], json!({"executed": "learn"}), "the answer: {:?}", h.replies(reply));
        assert!(sent.elapsed() < Duration::from_secs(1));
        // Progress: the silent device is retried, paced, as R1's outage machinery does.
        let silent = h.fleet.device(&bedroom).expect("the silent device was built");
        assert!(h.eventually(bound(), |_| silent.calls_of(Op::Connect).len() >= 2).await, "the silent device was never retried");
        assert_eq!(h.status_via(&salon).and_then(|(_, via)| via).as_deref(), Some(h.bridge.as_str()), "the Status was not tagged");
        assert!(h.stop().await, "the host did not stop in time");
    })
    .await
}

/// Retained, as the previous run left it: `t1`'s State `{"power": false}` (tagged with this
/// process's bridge id), the target's Config, and a property Desired under `conflict`.
fn previous_run(h: &HostHarness, instance: &str, conflict: &str) {
    let t = h.topics(instance);
    h.broker.retain(
        &t.state("t1"),
        json!({"power": false}).to_string(),
        &SendOptions::default().retained().user_property("via", &h.bridge),
    );
    h.broker.retain(&t.config("t1"), json!({"zone": 1}).to_string(), &SendOptions::default().retained());
    h.broker.retain(
        &t.desired("t1", "power"),
        json!(false).to_string(),
        &SendOptions::default().retained().user_property("conflict", conflict),
    );
}

/// The position of the first label starting with `prefix`.
fn position(labels: &[String], prefix: &str) -> Option<usize> {
    labels.iter().position(|l| l.starts_with(prefix))
}

/// Scenario 2. A Config naming this process subscribes the instance's filters — S1 (its
/// read-back), S2 (its target Configs), S3 (its requests), in order — and the broker's replay of
/// its retained target Configs and Desired bootstraps the device: the target Config is applied
/// first, and the retained Desired goes through the conflict rule against the read-back.
#[tokio::test(flavor = "multi_thread")]
async fn a_config_naming_this_process_bootstraps_the_device_from_its_own_replay() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let (kitchen, hall) = (instance_name("Kitchen"), instance_name("Hall"));
        // Changed at the device while the bridge was away: `power` is true there now.
        fleet.prepare(&kitchen, |d| d.set("t1", "power", json!(true)));
        fleet.prepare(&hall, |d| d.set("t1", "power", json!(true)));
        let h = HostHarness::start("acquire", fleet).await;
        previous_run(&h, &kitchen, "DeviceWins");
        previous_run(&h, &hall, "RequestWins");
        assert!(h.configure(&kitchen, json!({})) && h.configure(&hall, json!({})), "a Config reached no subscription");
        for instance in [&kitchen, &hall] {
            let t = h.topics(instance);
            let command = format!("sub {}", t.command());
            assert!(
                h.eventually(bound(), |h| position(&h.labels(), &command).is_some()).await,
                "{instance}'s requests were never subscribed: {:#?}",
                h.labels()
            );
            let labels = h.labels();
            let at = |filter: String| {
                position(&labels, &format!("sub {filter}")).unwrap_or_else(|| panic!("{filter} not subscribed: {labels:#?}"))
            };
            let (states, status, configs, desired) =
                (at(t.state_target_filter()), at(t.status()), at(t.config_target_filter()), at(t.desired_property_filter()));
            assert!(states.max(status) < configs && configs < desired, "S1, S2, S3 were not subscribed in order: {labels:#?}");
            assert!(position(&labels, &format!("sub {}", t.desired_target_filter())).is_some_and(|p| p > configs));
        }
        // DeviceWins: changed at the device since the read-back's `false` — superseded, never
        // applied.
        assert!(
            h.eventually(bound(), |h| h.errors_for(&kitchen, "superseded").iter().any(|e| e["target"] == "t1" && e["property"] == "power"))
                .await,
            "the replayed Desired was not decided against the read-back: {:?}",
            h.errors(&kitchen)
        );
        let device = h.fleet.device(&kitchen).expect("built");
        assert!(device.calls_of(Op::Apply).is_empty(), "a superseded request was applied: {:?}", device.applies());
        assert!(
            device.calls_of(Op::Configure).iter().any(|c| c.target.as_deref() == Some("t1")),
            "the target's retained Config was never applied"
        );
        // RequestWins: applied — after the target's Config.
        let device = h.fleet.device(&hall).expect("built");
        assert!(h.eventually(bound(), |_| !device.calls_of(Op::Apply).is_empty()).await, "the RequestWins replay was never applied");
        let calls = device.calls();
        let configured = calls.iter().position(|c| c.op == Op::Configure && c.target.as_deref() == Some("t1")).expect("configured");
        let applied = calls.iter().position(|c| c.op == Op::Apply).expect("applied");
        assert!(configured < applied, "the retained Desired was applied before the target's Config: {calls:#?}");
        assert_eq!(device.get("t1", "power"), Some(json!(false)));
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 4c. The read-back freezes at S2's SUBACK: a retained State behind S1's SUBACK is in the
/// conflict snapshot, one that arrives after S2's SUBACK is not; and the target Configs apply
/// before the retained Desired.
#[tokio::test(flavor = "multi_thread")]
async fn the_read_back_freezes_at_the_setup_subscriptions_suback() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let porch = instance_name("Porch");
        fleet.prepare(&porch, |d| d.set("t1", "power", json!(true)));
        let h = HostHarness::start("freeze", fleet).await;
        previous_run(&h, &porch, "DeviceWins");
        let t = h.topics(&porch);
        // S3's SUBACK held: S2's has come by the time S3 is subscribed.
        h.broker.hold_subacks_matching(&format!("{ROOT}/Desired/{porch}"));
        h.configure(&porch, json!({}));
        assert!(h.eventually(bound(), |h| h.subscribed(&t.command())).await, "S3 was never subscribed: {:#?}", h.labels());
        // A State flagged retained after S2's SUBACK — as a late replay would be — must not move the
        // "before" value: with it, the device's `true` would read as unchanged and the request apply.
        let late = SendOptions::default().retained().user_property("via", &h.bridge);
        assert!(h.broker.send_with(&t.state("t1"), json!({"power": true}).to_string(), &late));
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.broker.release_subacks();
        assert!(
            h.eventually(bound(), |h| !h.errors_for(&porch, "superseded").is_empty()).await,
            "the replayed Desired was decided against a State that came after S2's SUBACK: errors {:?}",
            h.errors(&porch)
        );
        let device = h.fleet.device(&porch).expect("built");
        assert!(device.calls_of(Op::Apply).is_empty(), "applied: {:?}", device.applies());
        assert!(
            device.calls_of(Op::Configure).iter().any(|c| c.target.as_deref() == Some("t1") && c.values == Some(json!({"zone": 1}))),
            "the target's retained Config was never applied: {:#?}",
            device.calls()
        );
        assert!(h.stop().await);
    })
    .await
}

/// Scenario 7. Fairness: a device producing State without pause never delays another device's
/// reply — devices are served round-robin, at most a quantum of messages a turn — nor starves its
/// own: its batch is sealed when taken, so its reply goes out with that batch, never behind State
/// reported later — at most after the batch under way and its own batch's State. The chatty device
/// reports 1200 targets over and over: each batch it gives takes the broker, slow but steady to
/// acknowledge, some 2-3 s.
#[tokio::test(flavor = "multi_thread")]
async fn a_device_reporting_without_pause_delays_no_reply() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        // The chatty one is acquired first: first in the rotation.
        let (chatty, quiet) = (instance_name("Achatty"), instance_name("Bquiet"));
        fleet.prepare(&chatty, |d| d.set("t0", "n", json!(0)));
        fleet.prepare(&quiet, |d| d.set("t0", "n", json!(0)));
        let h = HostHarness::start("fair", fleet).await;
        assert!(h.configure(&chatty, json!({})));
        assert!(h.eventually(bound(), |h| h.status(&chatty).as_deref() == Some("connected")).await, "the chatty device never connected");
        assert!(h.configure(&quiet, json!({})));
        assert!(h.eventually(bound(), |h| h.status(&quiet).as_deref() == Some("connected")).await, "the quiet device never connected");
        h.broker.delay_acks(Duration::from_millis(10));
        let device = h.fleet.device(&chatty).expect("built");
        let flood = tokio::spawn(async move {
            let mut n = 0u64;
            loop {
                for t in 0..1200 {
                    let mut values = mqtt_bridge_kit::PropertyMap::new();
                    values.insert("n".into(), json!(n));
                    device.report(&format!("t{t}"), values);
                }
                n += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        for round in 0..2 {
            // Another device's reply: within 1 s, a few quanta of the chatty one's batch.
            let reply = format!("Store/replies/fair-quiet-{round}");
            assert!(h.command(&quiet, json!({"command": "learn"}), Some((&reply, b"q"))));
            assert!(
                h.eventually(Duration::from_secs(1), |h| !h.replies(&reply).is_empty()).await,
                "the quiet device's reply was delayed past 1 s by the chatty device's State (round {round})"
            );
            // Its own: after the batch under way and its own batch's State — about two batches —
            // never behind State reported after it.
            let reply = format!("Store/replies/fair-chatty-{round}");
            assert!(h.command(&chatty, json!({"command": "learn"}), Some((&reply, b"c"))));
            assert!(
                h.eventually(Duration::from_secs(12), |h| !h.replies(&reply).is_empty()).await,
                "the chatty device's own reply was starved by the State it reported after it (round {round})"
            );
        }
        flood.abort();
        h.broker.delay_acks(Duration::ZERO);
        assert!(h.stop().await);
    })
    .await
}

/// The read-back of a configured instance takes only a `State` that carries exactly one `via`
/// (multi-device spec §3): an untagged retained `State` is no "before" value — here the request is
/// applied as a first install (no "before"), where reading it would have superseded it.
#[tokio::test(flavor = "multi_thread")]
async fn an_untagged_retained_state_is_no_read_back() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let porch = instance_name("Porch");
        fleet.prepare(&porch, |d| d.set("t1", "power", json!(true)));
        let h = HostHarness::start("untagged-read-back", fleet).await;
        let t = h.topics(&porch);
        h.broker.retain(&t.state("t1"), json!({"power": false}).to_string(), &SendOptions::default().retained());
        h.broker.retain(
            &t.desired("t1", "power"),
            json!(false).to_string(),
            &SendOptions::default().retained().user_property("conflict", "DeviceWins"),
        );
        assert!(h.configure(&porch, json!({})));
        let device = h.built(&porch).await;
        assert!(
            h.eventually(bound(), |_| device.get("t1", "power") == Some(json!(false))).await,
            "an untagged State was read back as a \"before\" value: errors {:?}",
            h.errors(&porch)
        );
        assert!(h.errors_for(&porch, "superseded").is_empty(), "{:?}", h.errors(&porch));
        assert!(h.stop().await);
    })
    .await
}

/// The read-back takes only a `State` tagged with the instance's effective owner (§4.2): one tagged
/// by another process is no "before" value — the request is applied as a first install, where
/// reading it would have superseded it (R2 gate, Codex 6).
#[tokio::test(flavor = "multi_thread")]
async fn a_foreign_tagged_retained_state_is_no_read_back() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let porch = instance_name("Porch");
        fleet.prepare(&porch, |d| d.set("t1", "power", json!(true)));
        let h = HostHarness::start("foreign-read-back", fleet).await;
        let t = h.topics(&porch);
        let foreign = SendOptions::default().retained().user_property("via", "another-process");
        h.broker.retain(&t.state("t1"), json!({"power": false}).to_string(), &foreign);
        h.broker.retain(
            &t.desired("t1", "power"),
            json!(false).to_string(),
            &SendOptions::default().retained().user_property("conflict", "DeviceWins"),
        );
        assert!(h.configure(&porch, json!({})));
        let device = h.built(&porch).await;
        assert!(
            h.eventually(bound(), |_| device.get("t1", "power") == Some(json!(false))).await,
            "a State another process tagged was read back as a \"before\" value: errors {:?}",
            h.errors(&porch)
        );
        assert!(h.errors_for(&porch, "superseded").is_empty(), "{:?}", h.errors(&porch));
        assert!(h.stop().await);
    })
    .await
}

/// Re-enabled, an instance seeds its read-back from what its slot shows — only a `State` tagged
/// with its effective owner (§4.2): one another process tagged while it was disabled is no
/// "before" value (R2 gate, Codex 6).
#[tokio::test(flavor = "multi_thread")]
async fn a_re_enabled_instance_reads_back_only_its_own_tag() {
    within(DEADLINE, async {
        let fleet = Fleet::new(acknowledged());
        let porch = instance_name("Porch");
        fleet.prepare(&porch, |d| d.set("t1", "power", json!(true)));
        let h = HostHarness::start("foreign-re-enable", fleet).await;
        let t = h.topics(&porch);
        assert!(h.configure(&porch, json!({"disabled": true})));
        assert!(h.eventually(bound(), |h| h.status(&porch).as_deref() == Some("disabled")).await, "{:?}", h.statuses(&porch));
        let foreign = SendOptions::default().retained().user_property("via", "another-process");
        assert!(h.broker.publish(&t.state("t1"), json!({"power": false}).to_string(), &foreign));
        h.broker.retain(
            &t.desired("t1", "power"),
            json!(false).to_string(),
            &SendOptions::default().retained().user_property("conflict", "DeviceWins"),
        );
        // The disabled instance's read-back has seen the foreign write.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(h.configure(&porch, json!({})));
        let device = h.built(&porch).await;
        assert!(
            h.eventually(bound(), |_| device.get("t1", "power") == Some(json!(false))).await,
            "a State another process tagged was seeded into the re-enabled read-back: errors {:?}",
            h.errors(&porch)
        );
        assert!(h.errors_for(&porch, "superseded").is_empty(), "{:?}", h.errors(&porch));
        assert!(h.stop().await);
    })
    .await
}

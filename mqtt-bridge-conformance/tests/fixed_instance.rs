//! A fixed bridge is the host's N = 1 mode (multi-device spec §4.1, §4.2): one fixed instance,
//! owned by the process itself — no `Config/+` ownership, no barrier — with R1's CONNACK duties
//! (Bridge Runtime spec §5, §9), now carried by the host's lifecycle.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use serde_json::json;

/// The outer bound on a scenario.
const DEADLINE: Duration = Duration::from_secs(60);

/// A read-back whose SUBACK does not come within its wait (5 s) is said incomplete, WARN
/// `read_back_incomplete` with the wait, and the session ends there: the abandoned SUBSCRIBE's
/// packet id stays in use until its SUBACK (MQTT §2.2.1), so going on would only freeze the
/// publication behind it until the 10 s grant bound ended the session anyway (the R2 confirming
/// reviews 2, Claude minor 2). The next connection subscribes the requests at once, without the
/// read-back — it is not asked again — and the bridge announces itself and publishes its model.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_read_back_is_said_incomplete_and_the_requests_go_without_it() {
    let scenario = async {
        let h = Harness::start_unwaited("late-read-back", acknowledged(), quick_timing(), |broker, topics, device| {
            broker.hold_subacks_matching(&topics.state_filter());
            device.set("t1", "temp", json!(20));
        })
        .await;
        let t = h.topics.clone();
        assert!(h.broker.wait_for_subscription(&t.state_filter(), BOUND).await, "the read-back was never subscribed");
        let asked = std::time::Instant::now();
        h.settle(Duration::from_millis(3000)).await;
        assert!(
            !h.broker.subscriptions().contains(&t.command()),
            "the requests were subscribed before the read-back's wait was up: {:?}",
            h.broker.subscriptions()
        );
        assert!(
            h.eventually(Duration::from_secs(5), |h| h.broker.subscriptions().contains(&t.command())).await,
            "the requests were never subscribed without the read-back: {:?}",
            h.broker.subscriptions()
        );
        // At the wait, not at the read-back SUBSCRIBE's 10 s grant bound.
        assert!(
            h.broker.connections() == 2 && asked.elapsed() < Duration::from_secs(8),
            "the session did not end at the read-back's wait: connections {}, {:?} after the read-back was asked",
            h.broker.connections(),
            asked.elapsed()
        );
        let warned = h.log.of_kind("read_back_incomplete");
        assert!(
            warned.len() == 1 && warned[0].level == tracing::Level::WARN && warned[0].field("waited_ms").is_some(),
            "a late read-back was not said incomplete, once, with its wait: {:#?}",
            h.log.records()
        );
        assert!(
            h.eventually(BOUND, |h| h.status().as_deref() == Some("connected") && h.state("t1").is_some()).await,
            "the bridge did not go on without the read-back: status {:?}",
            h.status()
        );
        assert_eq!(h.broker.retained_on(&t.active()).as_deref(), Some(&b"true"[..]), "the bridge never said Active=true");
        let asked_again = h.broker.events().iter().filter(|e| matches!(e, BrokerEvent::Subscribed(f) if *f == t.state_filter())).count();
        assert_eq!(asked_again, 1, "the read-back was asked again on the next connection");
        assert!(h.broker.pkid_reuses().is_empty(), "a packet id was reused: {:#?}", h.broker.pkid_reuses());
        h.broker.release_subacks();
        h.settle(Duration::from_millis(500)).await;
        assert_eq!(h.log.of_kind("read_back_incomplete").len(), 1, "the read-back was said incomplete again");
        assert_eq!(h.broker.connections(), 2, "the session did not hold once the read-back was given up");
        assert!(h.stop().await);
    };
    tokio::time::timeout(DEADLINE, scenario).await.expect("the scenario did not end within its deadline");
}

/// The read-back is complete at the first SUBACK of a request behind its replay, whichever request
/// it acknowledges, a refusal included (R1; Bridge Runtime spec §5): with the `Config/{i}/#` SUBACK
/// held, another request's SUBACK completes it, and it is unsubscribed before `Config/{i}/#` is
/// acknowledged (the R2 confirming reviews 2, Codex 1).
async fn the_read_back_completes_at_the_first_request_suback(name: &str, refused: bool) {
    let h = Harness::start_unwaited(name, acknowledged(), quick_timing(), |broker, topics, device| {
        broker.hold_subacks_matching(&topics.config_filter());
        if refused {
            broker.deny_subscriptions(&topics.desired_target_filter());
            broker.deny_subscriptions(&topics.command());
        }
        device.set("t1", "temp", json!(20));
    })
    .await;
    let t = h.topics.clone();
    // The first request, its SUBACK held (a refused subscription is not recorded as made).
    assert!(h.broker.wait_for_subscription(&t.config_filter(), BOUND).await, "the requests were never subscribed");
    let unsubscribed = |h: &Harness| h.broker.events().iter().any(|e| matches!(e, BrokerEvent::Unsubscribed(f) if *f == t.state_filter()));
    assert!(
        h.eventually(Duration::from_secs(3), unsubscribed).await,
        "the read-back waited for the Config SUBACK, held, though another request's SUBACK came (refused: {refused}): {:?}",
        h.broker.events()
    );
    assert!(h.log.of_kind("read_back_incomplete").is_empty(), "the read-back was said incomplete: {:#?}", h.log.records());
    h.broker.release_subacks();
    assert!(
        h.eventually(BOUND, |h| h.status().as_deref() == Some("connected") && h.state("t1").is_some()).await,
        "the bridge never published its model: status {:?}",
        h.status()
    );
    assert!(h.broker.pkid_reuses().is_empty(), "a packet id was reused: {:#?}", h.broker.pkid_reuses());
    assert!(h.stop().await);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_back_completes_at_the_first_request_suback_whichever_it_acknowledges() {
    tokio::time::timeout(DEADLINE, the_read_back_completes_at_the_first_request_suback("first-suback", false))
        .await
        .expect("the scenario did not end within its deadline");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_back_completes_at_a_refused_request_suback() {
    tokio::time::timeout(DEADLINE, the_read_back_completes_at_the_first_request_suback("refused-suback", true))
        .await
        .expect("the scenario did not end within its deadline");
}

/// A stop during the first read-back's wait: the fixed slot was never announced on the connection
/// — the read-back is still reading the retained State its model would overwrite — so the flush
/// publishes none of its model; `Active=false` and the DISCONNECT still go (the R2 confirming
/// reviews 2, Claude minor 3).
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_during_the_read_back_wait_publishes_no_model() {
    let scenario = async {
        let mut h = Harness::start_unwaited("stop-read-back-wait", acknowledged(), quick_timing(), |broker, topics, device| {
            broker.hold_subacks_matching(&topics.state_filter());
            device.set("t1", "temp", json!(20));
            device.set("t2", "temp", json!(21));
        })
        .await;
        let t = h.topics.clone();
        assert!(h.broker.wait_for_subscription(&t.state_filter(), BOUND).await, "the read-back was never subscribed");
        assert!(h.eventually(BOUND, |h| h.device.connected()).await, "the device never connected");
        h.settle(Duration::from_millis(500)).await;
        assert!(h.stop_bridge().await, "the bridge did not stop in time");
        let state_prefix = t.state_filter().trim_end_matches('#').to_owned();
        let model: Vec<String> = h
            .broker
            .events()
            .iter()
            .filter_map(|e| match e {
                BrokerEvent::Published(r) if r.topic == t.status() || r.topic.starts_with(&state_prefix) => Some(r.topic.clone()),
                _ => None,
            })
            .collect();
        assert!(model.is_empty(), "the flush published the model of a slot never announced: {model:?}");
        assert!(
            h.published(&t.active()).iter().any(|r| &r.payload[..] == b"false"),
            "the flush did not say Active=false: {:#?}",
            h.log.of_kind("bridge_shutdown_cut")
        );
        assert!(h.broker.pkid_reuses().is_empty(), "a packet id was reused: {:#?}", h.broker.pkid_reuses());
        h.broker.release_subacks();
    };
    tokio::time::timeout(DEADLINE, scenario).await.expect("the scenario did not end within its deadline");
}

/// A fixed bridge owns nothing by Config: it subscribes only its own instance's lanes — no
/// `Config/+`, no `Status/+` — whatever the root holds.
#[tokio::test(flavor = "multi_thread")]
async fn a_fixed_bridge_subscribes_only_its_own_lanes() {
    let scenario = async {
        let h = Harness::start("own-lanes", acknowledged(), quick_timing()).await;
        assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await, "the bridge never said it was connected");
        let instance = h.instance().to_owned();
        let foreign: Vec<String> = h
            .broker
            .events()
            .into_iter()
            .filter_map(|e| match e {
                BrokerEvent::Subscribed(f) if f.split('/').nth(2) != Some(instance.as_str()) => Some(f),
                _ => None,
            })
            .collect();
        assert!(foreign.is_empty(), "a fixed bridge subscribed beyond its own instance's lanes: {foreign:?}");
        assert!(h.stop().await);
    };
    tokio::time::timeout(DEADLINE, scenario).await.expect("the scenario did not end within its deadline");
}

//! A fixed bridge is the host's N = 1 mode (multi-device spec §4.1, §4.2): one fixed instance,
//! owned by the process itself — no `Config/+` ownership, no barrier — with R1's CONNACK duties
//! (Bridge Runtime spec §5, §9), now carried by the host's lifecycle.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use serde_json::json;

/// The outer bound on a scenario.
const DEADLINE: Duration = Duration::from_secs(60);

/// A read-back whose SUBACK does not come within its wait (5 s) is said incomplete, WARN
/// `read_back_incomplete` with the wait, and the requests are subscribed without it; it is
/// unsubscribed, and the bridge announces itself and publishes its model. Its SUBACK coming late
/// changes nothing.
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
        let warned = h.log.of_kind("read_back_incomplete");
        assert!(
            warned.len() == 1 && warned[0].level == tracing::Level::WARN && warned[0].field("waited_ms").is_some(),
            "a late read-back was not said incomplete, once, with its wait: {:#?}",
            h.log.records()
        );
        let unsubscribed =
            |h: &Harness| h.broker.events().iter().any(|e| matches!(e, BrokerEvent::Unsubscribed(f) if *f == t.state_filter()));
        assert!(
            h.eventually(BOUND, |h| unsubscribed(h) && h.status().as_deref() == Some("connected") && h.state("t1").is_some()).await,
            "the bridge did not go on without the read-back: unsubscribed {}, status {:?}",
            unsubscribed(&h),
            h.status()
        );
        assert_eq!(h.broker.retained_on(&t.active()).as_deref(), Some(&b"true"[..]), "the bridge never said Active=true");
        h.broker.release_subacks();
        h.settle(Duration::from_millis(500)).await;
        assert_eq!(h.log.of_kind("read_back_incomplete").len(), 1, "the late SUBACK was taken for a read-back again");
        assert_eq!(h.broker.connections(), 1, "the session did not hold through the late read-back");
        assert!(h.stop().await);
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

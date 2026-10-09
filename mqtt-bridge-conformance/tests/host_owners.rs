//! Two processes of one kind on one root (multi-device spec §7, scenarios 6 and 8): an instance's
//! `via` moved from one to the other, and one process's death. Each scenario has an outer
//! deadline and a progress assertion.

use std::sync::Arc;
use std::time::Duration;

use mqtt_bridge_conformance::host::*;
use mqtt_bridge_conformance::*;
use serde_json::json;

/// Each scenario's outer bound.
const DEADLINE: Duration = Duration::from_secs(60);

/// Two hosts of the kind on one broker.
async fn two_hosts(name: &str) -> (HostHarness, HostHarness) {
    let broker = Arc::new(FakeBroker::start_multi().await);
    broker.cap_connections(80);
    let first = HostHarness::join(broker.clone(), &instance_name(&format!("{name}-one")), Fleet::new(acknowledged()), |h| h).await;
    let second = HostHarness::join(broker, &instance_name(&format!("{name}-two")), Fleet::new(acknowledged()), |h| h).await;
    (first, second)
}

/// Scenario 6: moving `via` — the old owner releases the instance (stopped, unsubscribed, nothing
/// cleared), the new owner bootstraps it by its own subscriptions; a late `Status` from the old
/// owner is reasserted over by the new one.
#[tokio::test(flavor = "multi_thread")]
async fn moving_via_hands_the_instance_over_and_heals_a_late_write() {
    within(DEADLINE, async {
        let (one, two) = two_hosts("move").await;
        let lamp = instance_name("Lamp");
        one.fleet.prepare(&lamp, |d| d.set("t1", "power", json!(true)));
        two.fleet.prepare(&lamp, |d| d.set("t1", "power", json!(true)));
        assert!(one.configure(&lamp, json!({})));
        assert!(
            one.eventually(BOUND, |h| h.status_via(&lamp) == Some(("connected".into(), Some(h.bridge.clone())))).await,
            "the first owner never served the instance: {:?}",
            one.status_via(&lamp)
        );
        let old = one.fleet.device(&lamp).expect("built by the first owner");
        // The Store moves the instance: its Config now names the second process.
        assert!(two.configure(&lamp, json!({})));
        assert!(one.eventually(BOUND, |_| !old.calls_of(Op::Disconnect).is_empty()).await, "the old owner did not release the instance");
        assert!(
            two.eventually(BOUND, |h| h.status_via(&lamp) == Some(("connected".into(), Some(h.bridge.clone())))).await,
            "the new owner never served the instance: {:?}",
            two.status_via(&lamp)
        );
        assert_eq!(two.fleet.builds(&lamp), 1);
        // A late write from the old owner: the new owner reasserts its own.
        let t = two.topics(&lamp);
        let marked = two.statuses(&lamp).len();
        two.broker.publish(&t.status(), "unreachable", &SendOptions::default().retained().user_property("via", &one.bridge));
        assert!(
            two.eventually(BOUND, |h| h.statuses(&lamp).len() > marked
                && h.status_via(&lamp) == Some(("connected".into(), Some(h.bridge.clone()))))
                .await,
            "the new owner did not reassert its Status over the old owner's late write: {:?}",
            two.status_via(&lamp)
        );
        assert!(!two.log.of_kind("slot_reasserted").is_empty(), "{:#?}", two.log.records());
        assert_eq!(one.fleet.builds(&lamp), 1, "the old owner built the instance again");
        let (mut one, two) = (one, two);
        assert!(one.stop_host().await);
        assert!(two.stop().await);
    })
    .await
}

/// Scenario 8: a process's death — its connection gone without a DISCONNECT — makes its bridge id's
/// `Active` false by its last will (so its instances unavailable); the other process is unaffected.
#[tokio::test(flavor = "multi_thread")]
async fn a_process_death_makes_only_its_own_instances_unavailable() {
    within(DEADLINE, async {
        let (mut one, two) = two_hosts("death").await;
        let (x, y) = (instance_name("X"), instance_name("Y"));
        assert!(one.configure(&x, json!({})) && two.configure(&y, json!({})));
        assert!(
            one.eventually(BOUND, |h| h.status(&x).as_deref() == Some("connected") && h.status(&y).as_deref() == Some("connected")).await,
            "the instances never connected"
        );
        assert!(
            one.eventually(BOUND, |h| h.broker.retained_on(&h.active_topic()).as_deref() == Some(&b"true"[..])).await,
            "the first process never said Active=true"
        );
        one.abort();
        assert!(
            one.eventually(BOUND, |h| h.broker.retained_on(&h.active_topic()).as_deref() == Some(&b"false"[..])).await,
            "the dead process's last will did not say Active=false"
        );
        assert_eq!(two.broker.retained_on(&two.active_topic()).as_deref(), Some(&b"true"[..]), "the other process's Active changed");
        let reply = "Store/replies/death";
        assert!(two.command(&y, json!({"command": "learn"}), Some((reply, b"1"))));
        assert!(two.eventually(BOUND, |h| !h.replies(reply).is_empty()).await, "the other process stopped answering");
        assert_eq!(two.status(&y).as_deref(), Some("connected"));
        assert!(two.stop().await);
    })
    .await
}

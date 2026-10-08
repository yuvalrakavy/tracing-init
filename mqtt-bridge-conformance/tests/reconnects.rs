//! Reconnects (Bridge Runtime spec §9): every CONNACK subscribes, then announces `Active=true`,
//! then republishes the retained model from the bridge's own state — also to a broker that
//! restarted without its retained messages.

use mqtt_bridge_conformance::*;
use serde_json::json;

/// What the broker saw from the `n`-th connection on (1-based), as short labels.
fn from_connection(broker: &FakeBroker, n: usize) -> Vec<String> {
    let mut connections = 0;
    let mut labels = Vec::new();
    for event in broker.events() {
        if let BrokerEvent::Connected = event {
            connections += 1;
        }
        if connections < n {
            continue;
        }
        labels.push(match event {
            BrokerEvent::Connected => "connect".to_owned(),
            BrokerEvent::Subscribed(f) => format!("sub {f}"),
            BrokerEvent::Unsubscribed(f) => format!("unsub {f}"),
            BrokerEvent::Published(r) => format!("pub {} {}", r.topic, String::from_utf8_lossy(&r.payload)),
            BrokerEvent::Disconnected => "disconnect".to_owned(),
        });
    }
    labels
}

fn position(labels: &[String], prefix: &str) -> Option<usize> {
    labels.iter().position(|l| l.starts_with(prefix))
}

/// The first CONNACK subscribes to the bridge's own retained `State` (the read-back) before its
/// lanes; every CONNACK subscribes to `Desired`, `Command` and `Config` before `Active=true`, then
/// publishes the version, the status and every target's state again.
#[tokio::test(flavor = "multi_thread")]
async fn a_connack_subscribes_before_active_then_republishes_the_model() {
    let h = Harness::start_with("connack", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "power", json!(true))).await;
    let t = h.topics.clone();
    assert!(
        h.eventually(BOUND, |h| h.state("t1").is_some() && h.status().as_deref() == Some("connected")).await,
        "the device's state never reached the broker"
    );
    let first = from_connection(&h.broker, 1);
    let read_back = position(&first, &format!("sub {}", t.state_filter())).expect("the first CONNACK made no read-back subscription");
    let desired = position(&first, &format!("sub {}", t.desired_target_filter())).expect("no Desired subscription");
    assert!(read_back < desired, "the read-back's subscription did not come first: {first:#?}");
    // Nothing is published before the subscriptions, the first publish is Active=true: a state
    // published sooner would overwrite the retained state the read-back is there to read.
    let first_publish = position(&first, "pub ").expect("nothing was published");
    let last_sub = first.iter().rposition(|l| l.starts_with("sub ")).expect("no subscription");
    assert!(last_sub < first_publish, "the bridge published before it had subscribed: {first:#?}");
    assert!(first[first_publish].starts_with(&format!("pub {} true", t.active())), "the first publish was not Active=true: {first:#?}");

    h.broker.drop_connection();
    assert!(
        h.eventually(BOUND, |h| {
            let again = from_connection(&h.broker, 2);
            position(&again, &format!("pub {} ", t.state("t1"))).is_some()
        })
        .await,
        "the state was not published again after the reconnect: {:#?}",
        from_connection(&h.broker, 2)
    );
    let again = from_connection(&h.broker, 2);
    let active = position(&again, &format!("pub {} true", t.active())).expect("no Active=true after the reconnect");
    for filter in [t.desired_target_filter(), t.desired_property_filter(), t.command(), t.config_filter()] {
        let sub = position(&again, &format!("sub {filter}")).unwrap_or_else(|| panic!("{filter} was not subscribed again: {again:#?}"));
        assert!(sub < active, "{filter} was subscribed after Active=true: {again:#?}");
    }
    assert!(position(&again, &format!("sub {}", t.state_filter())).is_none(), "the read-back was subscribed again: {again:#?}");
    for model in [format!("pub {} ", t.version()), format!("pub {} connected", t.status()), format!("pub {} ", t.state("t1"))] {
        let at = position(&again, &model).unwrap_or_else(|| panic!("`{model}` was not published again: {again:#?}"));
        assert!(at > active, "`{model}` came before Active=true: {again:#?}");
    }
    assert!(h.stop().await);
}

/// A broker restarted without persistence lost every retained message: the next CONNACK restores
/// the bridge's whole retained model.
#[tokio::test(flavor = "multi_thread")]
async fn a_broker_restarted_without_persistence_gets_the_retained_model_again() {
    let h = Harness::start_with("restart", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "power", json!(true))).await;
    let t = h.topics.clone();
    assert!(h.eventually(BOUND, |h| h.broker.retained_on(&t.state("t1")).is_some() && h.status().as_deref() == Some("connected")).await);
    h.broker.restart_without_persistence();
    assert!(h.broker.retained().is_empty());
    let restored = h
        .eventually(BOUND, |h| {
            [t.active(), t.status(), t.version(), t.state("t1")].iter().all(|topic| h.broker.retained_on(topic).is_some())
        })
        .await;
    assert!(restored, "the retained model was not restored after a broker restart; retained: {:?}", h.broker.retained());
    assert_eq!(h.broker.retained_on(&t.active()).as_deref(), Some(&b"true"[..]));
    assert_eq!(h.broker.retained_on(&t.status()).as_deref(), Some(&b"connected"[..]));
    assert!(h.stop().await);
}

/// On a reconnect, as on the first connection, nothing goes out before the subscriptions and
/// `Active=true`: a state reported while the broker was away, already handed to the client, must
/// not reach the broker ahead of them (review MINOR 14).
#[tokio::test(flavor = "multi_thread")]
async fn a_reconnect_publishes_nothing_before_its_subscriptions() {
    let h = Harness::start_with("requeued", acknowledged(), quick_timing(), |_, _, device| device.set("t1", "n", json!(0))).await;
    let t = h.topics.clone();
    assert!(h.eventually(BOUND, |h| h.state("t1").is_some() && h.status().as_deref() == Some("connected")).await);
    h.broker.refuse_connections(true);
    h.broker.drop_connection();
    // While the broker is away, the device reports, every 10 ms, for 600 ms; the broker takes
    // connections again half-way through.
    let device = h.device.clone();
    let reporting = tokio::spawn(async move {
        for n in 1..=60 {
            let values: mqtt_bridge_kit::PropertyMap = [("n".to_owned(), json!(n))].into_iter().collect();
            device.report("t1", values);
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    h.broker.refuse_connections(false);
    let _ = reporting.await;
    assert!(
        h.eventually(BOUND, |h| position(&from_connection(&h.broker, 2), &format!("pub {} true", t.active())).is_some()).await,
        "the bridge never announced itself again: {:#?}",
        from_connection(&h.broker, 2)
    );
    let again = from_connection(&h.broker, 2);
    let first_publish = position(&again, "pub ").expect("nothing was published");
    let last_sub = again.iter().rposition(|l| l.starts_with("sub ")).expect("no subscription");
    assert!(last_sub < first_publish, "something went out before the subscriptions on the reconnect: {again:#?}");
    assert!(again[first_publish].starts_with(&format!("pub {} true", t.active())), "the first publish was not Active=true: {again:#?}");
    assert!(h.stop().await);
}

/// The bridge subscribes to its requests only once the read-back's subscription is acknowledged,
/// so the broker's retained `State` is queued ahead of every retained request on the one stream —
/// the "before" values are in before any replay (review C9).
#[tokio::test(flavor = "multi_thread")]
async fn requests_are_subscribed_only_once_the_read_back_is_acknowledged() {
    let h = Harness::start_unwaited("readback-order", acknowledged(), quick_timing(), |broker, _, device| {
        broker.hold_subacks();
        device.set("t1", "power", json!(true));
    })
    .await;
    let t = h.topics.clone();
    assert!(h.broker.wait_for_subscription(&t.state_filter(), BOUND).await, "the read-back was never subscribed");
    h.settle(std::time::Duration::from_millis(500)).await;
    let early: Vec<String> = h.broker.subscriptions().into_iter().filter(|f| *f != t.state_filter()).collect();
    assert!(early.is_empty(), "requests were subscribed before the read-back was acknowledged: {early:?}");
    h.broker.release_subacks();
    assert!(h.broker.wait_for_subscription(&t.command(), BOUND).await, "the requests were never subscribed");
    assert!(h.stop().await);
}

/// A read-back subscription the broker refuses (an ACL) is no read-back: said at once, WARN
/// `read_back_incomplete` — not taken for a SUBACK that granted it (the R1 re-gate's minor: any
/// SUBACK counted, its packet id and reason codes unread).
#[tokio::test(flavor = "multi_thread")]
async fn a_read_back_the_broker_refuses_is_said_incomplete() {
    let h = Harness::start_unwaited("denied-read-back", acknowledged(), quick_timing(), |broker, topics, device| {
        device.set("t1", "temp", json!(20));
        broker.deny_subscriptions(&format!("{}/State/", topics.root()));
    })
    .await;
    assert!(h.broker.wait_for_subscription(&h.topics.command(), BOUND).await, "the bridge never subscribed to its requests");
    let warned = h.log.of_kind("read_back_incomplete");
    assert!(
        warned.len() == 1 && warned[0].level == tracing::Level::WARN,
        "a refused read-back was not said incomplete: {:#?}",
        h.log.records()
    );
    assert!(h.stop().await);
}

/// `Config` is subscribed before `Desired`, so a target's retained setup is queued ahead of its
/// retained values on the stream (review M4) — read from the broker's own record of the
/// subscriptions, in order, not from a race between the replays and the worker.
#[tokio::test(flavor = "multi_thread")]
async fn config_is_subscribed_before_desired() {
    let h = Harness::start("sub-order", acknowledged(), quick_timing()).await;
    assert!(h.broker.wait_for_subscription(&h.topics.command(), BOUND).await, "the bridge never subscribed");
    let subscribed: Vec<String> = h
        .broker
        .events()
        .iter()
        .filter_map(|e| match e {
            BrokerEvent::Subscribed(f) => Some(f.clone()),
            _ => None,
        })
        .collect();
    let at = |filter: String| subscribed.iter().position(|f| *f == filter);
    let config = at(h.topics.config_filter()).expect("Config was never subscribed");
    let desired = at(h.topics.desired_property_filter()).expect("Desired was never subscribed");
    assert!(config < desired, "Desired was subscribed before Config: {subscribed:?}");
    assert!(h.stop().await);
}

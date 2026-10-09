//! The session on its own, against FakeBroker: what it does with a publish rumqttc refuses.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use mqtt_test_broker::FakeBroker;

use super::*;
use crate::mailbox::Mailbox;
use crate::pump::BrokerOutage;
use crate::reports::Reports;

/// The MQTT side's shared state, for `broker`.
fn shared_for(broker: &FakeBroker) -> Shared {
    Shared {
        topics: Topics::new("T", "session").expect("topics"),
        broker_host: "127.0.0.1".into(),
        broker_port: broker.port(),
        timing: MqttTiming::default(),
        version: "test".into(),
        commands: Vec::new(),
        mailbox: Mailbox::new(),
        reports: Reports::new(),
        outage: Arc::new(BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5))),
        read_back_requested: AtomicBool::new(false),
        read_back_next: AtomicBool::new(false),
    }
}

/// A publish rumqttc refuses (a topic holding a wildcard) is one message dropped: the ones after it
/// still go out (review B2: the first refusal ended the CONNACK duties, and the session never
/// published a report again).
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_publish_is_one_message_dropped() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let shared = shared_for(&broker);
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    let model = Model { status: None, states: vec![("a#b".to_owned(), b"{}".to_vec()), ("t1".to_owned(), br#"{"x":1}"#.to_vec())] };
    let republished = tokio::time::timeout(Duration::from_secs(5), republish(&shared, &client, model)).await;
    assert!(republished.is_ok(), "the republish did not end");
    let t1 = shared.topics.state("t1");
    assert!(
        broker.wait_until(Duration::from_secs(5), |b| !b.received_on(&t1).is_empty()).await,
        "the state after a refused one was never published"
    );
}

/// Only a retained `State` is read back: a live one — the bridge's own publish coming back on its
/// read-back subscription, or another client's — is no record of what the previous run knew, and
/// no "before" value for the conflict rule.
#[test]
fn a_live_state_is_not_read_back() {
    let shared = shared_for_port();
    let topic = shared.topics.state("t1");
    let live = Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"temp":1}"#.to_vec(), None);
    route(&shared, &topic, &live, Instant::now());
    assert!(shared.reports.read_back_before().1.is_empty(), "a live State was read back");
    let mut retained = Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"temp":2}"#.to_vec(), None);
    retained.retain = true;
    route(&shared, &topic, &retained, Instant::now());
    assert_eq!(shared.reports.read_back_before().1["t1"]["temp"], serde_json::json!(2));
}

/// The shared state of a session that never connects: for routing alone.
fn shared_for_port() -> Shared {
    Shared {
        topics: Topics::new("T", "session").expect("topics"),
        broker_host: "127.0.0.1".into(),
        broker_port: 1,
        timing: MqttTiming::default(),
        version: "test".into(),
        commands: Vec::new(),
        mailbox: Mailbox::new(),
        reports: Reports::new(),
        outage: Arc::new(BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5))),
        read_back_requested: AtomicBool::new(false),
        read_back_next: AtomicBool::new(false),
    }
}

/// The session's panic seam is off without the feature (the re-gate 3 S5).
#[cfg(not(feature = "test-seams"))]
#[test]
fn the_panic_seam_is_off_without_its_feature() {
    std::env::set_var(TEST_PANIC_TOPIC, "T/seam");
    let ran = std::panic::catch_unwind(|| panic_for_test("T/seam"));
    std::env::remove_var(TEST_PANIC_TOPIC);
    assert!(ran.is_ok(), "the panic seam is on in a build without the test-seams feature");
}

/// A retraction settles an unproven assumption of its key: the Store saw the assumed State and
/// confirmed it, so the conflict rule takes the assumed value, not the one before it (re-gate 3 S6).
#[test]
fn a_retraction_settles_the_keys_unproven_assumption() {
    let shared = shared_for_port();
    shared.reports.assume_unproven("t1", serde_json::json!({"level": 40}).as_object().unwrap().clone());
    assert_eq!(shared.reports.values_of("t1").and_then(|v| v.get("level").cloned()), None);
    let topic = shared.topics.desired("t1", "level");
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&shared, &topic, &retraction, Instant::now());
    assert_eq!(
        shared.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        Some(serde_json::json!(40)),
        "a retraction left the key's assumption unproven"
    );
}

/// A retraction of a key whose write the mailbox holds unproven settles nothing: the probe proves
/// it, or the link's loss holds it again (re-gate 4 minor 7).
#[test]
fn a_retraction_of_an_unproven_write_leaves_its_assumption_unproven() {
    let shared = shared_for_port();
    shared.mailbox.connected();
    let level = serde_json::json!({"level": 40}).as_object().unwrap().clone();
    let topic = shared.topics.desired("t1", "level");
    let mut request = Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), None);
    request.retain = false;
    route(&shared, &topic, &request, Instant::now());
    assert!(shared.mailbox.try_take().is_some());
    shared.mailbox.applied(true);
    shared.reports.assume_unproven("t1", level);
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&shared, &topic, &retraction, Instant::now());
    assert_eq!(
        shared.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        None,
        "a retraction proved a write the device never proved taken"
    );
}

/// The same after the link was lost: the write is held again, unproven, and a confirmation then
/// settles nothing (Codex final gate 5).
#[test]
fn a_retraction_after_the_re_hold_leaves_its_assumption_unproven() {
    let shared = shared_for_port();
    shared.mailbox.connected();
    let level = serde_json::json!({"level": 40}).as_object().unwrap().clone();
    let topic = shared.topics.desired("t1", "level");
    let request = Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), None);
    route(&shared, &topic, &request, Instant::now());
    assert!(shared.mailbox.try_take().is_some());
    shared.mailbox.applied(true);
    shared.reports.assume_unproven("t1", level);
    let _ = shared.mailbox.disconnected();
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&shared, &topic, &retraction, Instant::now());
    assert_eq!(
        shared.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        None,
        "a confirmation after the re-hold proved a write the device never took"
    );
}

/// The stop's flush publishes what is pending — here an `Error` no refusal made — before its last
/// words, `Active=false` (re-gate R15; the final gate's refusals now go out apart from it, so this
/// is the flush's own proof).
#[tokio::test(flavor = "multi_thread")]
async fn the_flush_publishes_what_is_pending_before_its_last_words() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let shared = shared_for(&broker);
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    shared.reports.error(crate::wire::ErrorReport::new(crate::wire::Reason::Rejected, "pending at the stop").target(Some("t1")));
    let flushed = tokio::time::timeout(Duration::from_secs(5), flush(&shared, &client, &mut incoming)).await;
    assert!(flushed.is_ok(), "the flush did not end");
    let (error, active) = (shared.topics.error(), shared.topics.active());
    assert!(broker.wait_until(Duration::from_secs(5), |b| !b.received_on(&active).is_empty()).await, "the flush never said Active=false");
    let received = broker.received();
    let error_at = received.iter().position(|r| r.topic == error && String::from_utf8_lossy(&r.payload).contains("pending at the stop"));
    let active_at = received.iter().position(|r| r.topic == active);
    assert!(error_at.is_some(), "the flush dropped what was pending");
    assert!(error_at < active_at, "what was pending went out after the last words");
}

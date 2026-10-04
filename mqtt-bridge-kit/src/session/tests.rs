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

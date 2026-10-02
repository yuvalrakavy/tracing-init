//! The broker's own control: a real rumqttc v5 client connects, subscribes, receives, publishes —
//! and, with the acks held, saturates exactly as the bridges' tests need.

use std::time::Duration;

use rumqttc::v5::{AsyncClient, Event, MqttOptions};

use super::*;

const BOUND: Duration = Duration::from_secs(10);

fn client(broker: &FakeBroker, capacity: usize) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let mut options = MqttOptions::new("test-client", "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(5));
    AsyncClient::new(options, capacity)
}

/// Poll the event loop in a task of its own, collecting incoming publishes' payloads.
fn pump(mut events: rumqttc::v5::EventLoop) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<Bytes>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match events.poll().await {
                Ok(Event::Incoming(rumqttc::v5::Incoming::Publish(p))) => {
                    let _ = tx.send(p.payload);
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    (task, rx)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_connects_subscribes_receives_and_publishes() {
    let broker = FakeBroker::start().await;
    let (client, events) = client(&broker, 10);
    let (task, mut incoming) = pump(events);
    client.subscribe("in/#", QoS::AtLeastOnce).await.unwrap();
    assert!(broker.wait_for_subscription("in/#", BOUND).await, "{:?}", broker.subscriptions());
    assert!(broker.send("in/x", "hello"));
    let got = tokio::time::timeout(BOUND, incoming.recv()).await.unwrap().unwrap();
    assert_eq!(&got[..], b"hello");
    client.publish("out/y", QoS::AtLeastOnce, true, "reply").await.unwrap();
    assert!(broker.wait_until(BOUND, |b| b.received_on("out/y").len() == 1).await);
    assert!(broker.received_on("out/y")[0].retain);
    assert_eq!(broker.connections(), 1);
    task.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn held_acks_saturate_the_request_channel_and_releasing_them_drains_it() {
    // receive_max 2 and a channel of 4: with the acks held, the client takes 2 publishes in
    // flight and 4 more fill its channel; the seventh `publish` waits — and returns once the acks
    // are released, because this client's event loop is still being polled.
    let broker = FakeBroker::start_with_receive_max(2).await;
    let (client, events) = client(&broker, 4);
    let (task, _incoming) = pump(events);
    client.subscribe("in", QoS::AtMostOnce).await.unwrap();
    assert!(broker.wait_for_subscription("in", BOUND).await);
    broker.hold_acks();
    let mut published = 0;
    let saturated = loop {
        match tokio::time::timeout(Duration::from_millis(500), client.publish("out", QoS::AtLeastOnce, false, "x")).await {
            Ok(r) => {
                r.unwrap();
                published += 1;
                assert!(published < 100, "the channel never filled: the acks are not being held");
            }
            Err(_) => break published,
        }
    };
    assert!(broker.held_acks() >= 1, "nothing was held");
    assert!(saturated <= 8, "saturated after {saturated} publishes");
    broker.release_acks();
    tokio::time::timeout(BOUND, client.publish("out", QoS::AtLeastOnce, false, "after"))
        .await
        .expect("a polled client drains once the acks are released")
        .unwrap();
    assert!(broker.wait_until(BOUND, |b| b.received_on("out").len() == saturated + 1).await, "{}", broker.received_on("out").len());
    task.abort();
}

/// The same two properties over MQTT 3.1.1 (rumqttc's `v4`), where the in-flight limit is the
/// client's own.
#[tokio::test(flavor = "multi_thread")]
async fn a_v4_client_works_and_saturates_on_held_acks() {
    use rumqttc::{AsyncClient as Client4, Event as Event4, Incoming as Incoming4, MqttOptions as Options4};
    let broker = FakeBroker::start_v4().await;
    let mut options = Options4::new("test-client-4", "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(5)).set_inflight(2);
    let (client, mut events) = Client4::new(options, 4);
    let (tx, mut incoming) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match events.poll().await {
                Ok(Event4::Incoming(Incoming4::Publish(p))) => {
                    let _ = tx.send(p.payload);
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    client.subscribe("in", rumqttc::QoS::AtLeastOnce).await.unwrap();
    assert!(broker.wait_for_subscription("in", BOUND).await, "{:?}", broker.subscriptions());
    assert!(broker.send("in", "hello"));
    let got = tokio::time::timeout(BOUND, incoming.recv()).await.unwrap().unwrap();
    assert_eq!(&got[..], b"hello");

    broker.hold_acks();
    let mut published = 0;
    let saturated = loop {
        match tokio::time::timeout(Duration::from_millis(500), client.publish("out", rumqttc::QoS::AtLeastOnce, false, "x")).await {
            Ok(r) => {
                r.unwrap();
                published += 1;
                assert!(published < 100, "the v4 channel never filled: the acks are not being held");
            }
            Err(_) => break published,
        }
    };
    assert!(saturated <= 8, "saturated after {saturated} publishes");
    broker.release_acks();
    tokio::time::timeout(BOUND, client.publish("out", rumqttc::QoS::AtLeastOnce, false, "after"))
        .await
        .expect("a polled v4 client drains once the acks are released")
        .unwrap();
    assert!(broker.wait_until(BOUND, |b| b.received_on("out").len() == saturated + 1).await);
    task.abort();
}

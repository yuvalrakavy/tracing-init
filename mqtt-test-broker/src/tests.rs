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

/// Join `h`, or fail the test if it has not finished within [`BOUND`].
fn join_within<T>(h: std::thread::JoinHandle<T>, what: &str) -> std::thread::Result<T> {
    let deadline = std::time::Instant::now() + BOUND;
    while !h.is_finished() {
        assert!(std::time::Instant::now() < deadline, "{what} did not finish within {BOUND:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    h.join()
}

/// Review finding X-7 / C-20: an ack is decided and held under the `held` lock, which the hold
/// flag changes under, so a release cannot slip between the decision and the push and strand the
/// ack. The probe holds that lock while another thread decides an ack, turns the hold off under it
/// — as a release does — and only then lets the ack go on: it must see the release, not a hold it
/// read before.
#[test]
fn an_ack_decided_during_a_release_is_not_stranded() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let broker = rt.block_on(FakeBroker::start());
    let shared = broker.shared.clone();
    broker.hold_acks();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let held = shared.held.lock().unwrap();
    let s = shared.clone();
    let acker = std::thread::spawn(move || ack_or_hold(&s, &tx, 7));
    // The acker is now blocked on the lock this thread holds.
    std::thread::sleep(Duration::from_millis(100));
    shared.hold_acks.store(false, Ordering::SeqCst); // a release, made under the lock
    drop(held);
    join_within(acker, "the acker").unwrap();
    let stranded = shared.held.lock().unwrap().clone();
    assert!(stranded.is_empty(), "an ack decided during a release was stranded: held {stranded:?}");
    assert!(rx.try_recv().is_ok(), "the ack was sent at once");
}

/// Review finding C-19: dropping the broker stops it — its connections too, which used to go on
/// acknowledging and holding the client's socket open.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_broker_closes_its_connections() {
    let broker = FakeBroker::start().await;
    let mut stream = tokio::time::timeout(BOUND, tokio::net::TcpStream::connect(broker.address())).await.expect("connect").unwrap();
    let connect = v5::Connect { keep_alive: 30, client_id: "raw".into(), clean_start: true, properties: None };
    tokio::time::timeout(BOUND, stream.write_all(&encode_v5(v5::Packet::Connect(connect, None, None))))
        .await
        .expect("send the CONNECT")
        .unwrap();
    let mut buf = [0u8; 256];
    let n = tokio::time::timeout(BOUND, stream.read(&mut buf)).await.expect("a CONNACK").unwrap();
    assert!(n > 0 && buf[0] >> 4 == 2, "a CONNACK: {:?}", &buf[..n]);
    assert!(broker.wait_until(BOUND, |b| b.connections() == 1).await);
    drop(broker);
    match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("the dropped broker still wrote to its client: {:?}", &buf[..n]),
        Err(_) => panic!("the broker was dropped but its connection still holds the client's socket open"),
    }
}

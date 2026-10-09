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
/// ack. The probe holds that lock while another thread decides an ack, waits until that thread has
/// reached the lock (so it has decided whatever it decides before it), turns the hold off under it
/// — as a release does — and only then lets the ack go on: it must see the release, not a hold it
/// read before. Review finding T1: the wait is on the lock's arrival count, not a sleep, so a slow
/// acker is seen as surely as a prompt one.
#[test]
fn an_ack_decided_during_a_release_is_not_stranded() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let broker = rt.block_on(FakeBroker::start());
    let shared = broker.shared.clone();
    broker.hold_acks();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let held = shared.held.lock().unwrap();
    let reached = shared.held_lock_reached.load(Ordering::SeqCst);
    let s = shared.clone();
    let acker = std::thread::spawn(move || ack_or_hold(&s, &tx, 7));
    let deadline = std::time::Instant::now() + BOUND;
    while shared.held_lock_reached.load(Ordering::SeqCst) == reached {
        assert!(std::time::Instant::now() < deadline, "the acker did not reach the held lock within {BOUND:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
    // The acker is at the lock this thread holds, or about to block on it.
    shared.hold_acks.store(false, Ordering::SeqCst); // a release, made under the lock
    drop(held);
    join_within(acker, "the acker").unwrap();
    let stranded = shared.held.lock().unwrap().clone();
    assert!(stranded.is_empty(), "an ack decided during a release was stranded: held {stranded:?}");
    assert!(rx.try_recv().is_ok(), "the ack was sent at once");
}

/// A raw TCP client of `broker`, connected: its CONNECT sent and the CONNACK read.
async fn raw_client(broker: &FakeBroker) -> tokio::net::TcpStream {
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
    stream
}

/// Review finding C-19: dropping the broker stops it — its connections too, which used to go on
/// acknowledging and holding the client's socket open.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_broker_closes_its_connections() {
    let broker = FakeBroker::start().await;
    let mut stream = raw_client(&broker).await;
    let mut buf = [0u8; 256];
    drop(broker);
    match tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await {
        Ok(Ok(0)) | Ok(Err(_)) => {}
        Ok(Ok(n)) => panic!("the dropped broker still wrote to its client: {:?}", &buf[..n]),
        Err(_) => panic!("the broker was dropped but its connection still holds the client's socket open"),
    }
}

/// Review finding T4: a connection's writer goes with its connection, also when it is blocked in
/// `write_all` on a client that has stopped reading. Detached, it held the client's socket open
/// past `drop(broker)`, and once the client read again it delivered everything still queued.
#[tokio::test(flavor = "multi_thread")]
async fn dropping_the_broker_stops_a_writer_blocked_on_its_client() {
    let broker = FakeBroker::start().await;
    let mut stream = raw_client(&broker).await;
    // Far more than the two sockets' buffers hold (a few MB each, at most, on macOS and Linux),
    // queued while the client reads nothing: the writer blocks in `write_all`.
    const CHUNK: usize = 1 << 20;
    const CHUNKS: usize = 64;
    let payload = Bytes::from(vec![0u8; CHUNK]);
    for _ in 0..CHUNKS {
        assert!(broker.send("big", payload.clone()), "the client is connected");
    }
    drop(broker);
    // Now read to the end: what the sockets held at the drop, then the close.
    let mut buf = vec![0u8; 1 << 16];
    let mut read = 0usize;
    let ended = tokio::time::timeout(BOUND, async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => read += n,
            }
        }
    })
    .await;
    let queued = CHUNK * CHUNKS;
    assert!(read < queued / 2, "the dropped broker's writer went on writing: the client read {read} of the {queued} bytes queued for it");
    assert!(ended.is_ok(), "the broker was dropped but a writer still holds the client's socket open ({read} bytes read)");
}

/// Poll the event loop in a task of its own, collecting every incoming publish whole.
fn pump_publishes(mut events: rumqttc::v5::EventLoop) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<v5::Publish>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match events.poll().await {
                Ok(Event::Incoming(rumqttc::v5::Incoming::Publish(p))) => {
                    let _ = tx.send(p);
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    (task, rx)
}

async fn next_publish(incoming: &mut mpsc::UnboundedReceiver<v5::Publish>) -> v5::Publish {
    tokio::time::timeout(BOUND, incoming.recv()).await.expect("a publish within the bound").expect("the pump is running")
}

/// A retained message — the test's own, or one the client published — is replayed on a matching
/// subscription, flagged retained and with what is left of its expiry; an empty retained publish
/// clears it; an expired one is not replayed.
#[tokio::test(flavor = "multi_thread")]
async fn retained_messages_are_replayed_on_subscription_with_their_remaining_expiry() {
    let broker = FakeBroker::start().await;
    broker.retain("r/kept", "kept", &SendOptions::default().expiry(60).user_property("conflict", "DeviceWins"));
    broker.retain("r/cleared", "gone", &SendOptions::default());
    broker.retain("r/cleared", "", &SendOptions::default());
    broker.retain("r/expired", "old", &SendOptions::default().expiry(0));
    let (client, events) = client(&broker, 10);
    let (task, mut incoming) = pump_publishes(events);
    client.publish("r/own", QoS::AtLeastOnce, true, "own").await.unwrap();
    assert!(broker.wait_until(BOUND, |b| b.retained_on("r/own").is_some()).await, "the client's retained publish was not kept");
    client.subscribe("r/#", QoS::AtLeastOnce).await.unwrap();
    let mut replayed = Vec::new();
    for _ in 0..2 {
        replayed.push(next_publish(&mut incoming).await);
    }
    let topics: Vec<String> = replayed.iter().map(|p| String::from_utf8_lossy(&p.topic).into_owned()).collect();
    assert_eq!(topics, ["r/kept", "r/own"], "the replay is not the live retained messages, in topic order");
    assert!(replayed.iter().all(|p| p.retain), "a replayed message was not flagged retained");
    let kept = replayed[0].properties.clone().expect("the kept message's properties");
    assert!(matches!(kept.message_expiry_interval, Some(59..=60)), "the remaining expiry: {:?}", kept.message_expiry_interval);
    assert_eq!(kept.user_properties, vec![("conflict".to_owned(), "DeviceWins".to_owned())]);
    assert!(tokio::time::timeout(Duration::from_millis(300), incoming.recv()).await.is_err(), "a cleared or expired message was replayed");
    task.abort();
}

/// A publish from the client goes back to it on a matching subscription of its current
/// connection, unflagged; one the broker sends carries its v5 properties; one the client sends
/// keeps them in `received`.
#[tokio::test(flavor = "multi_thread")]
async fn publishes_are_routed_and_keep_their_properties_both_ways() {
    let broker = FakeBroker::start().await;
    let (client, events) = client(&broker, 10);
    let (task, mut incoming) = pump_publishes(events);
    client.subscribe("echo/#", QoS::AtLeastOnce).await.unwrap();
    assert!(broker.wait_for_subscription("echo/#", BOUND).await);
    let properties = PublishProperties { correlation_data: Some(Bytes::from_static(b"c-1")), ..PublishProperties::default() };
    client.publish_with_properties("echo/a", QoS::AtLeastOnce, true, "x", properties).await.unwrap();
    let echoed = next_publish(&mut incoming).await;
    assert_eq!(&echoed.topic[..], b"echo/a");
    assert!(!echoed.retain, "a forwarded publish kept its retain flag");
    assert!(broker.wait_until(BOUND, |b| b.received_on("echo/a").len() == 1).await);
    assert_eq!(broker.received_on("echo/a")[0].correlation_data(), Some(&Bytes::from_static(b"c-1")));

    let options =
        SendOptions::default().expiry(30).response_topic("echo/reply").correlation_data("c-2").user_property("conflict", "RequestWins");
    assert!(broker.send_with("in/x", "y", &options));
    let sent = next_publish(&mut incoming).await;
    let p = sent.properties.expect("the sent publish's properties");
    assert_eq!(p.message_expiry_interval, Some(30));
    assert_eq!(p.response_topic.as_deref(), Some("echo/reply"));
    assert_eq!(p.correlation_data, Some(Bytes::from_static(b"c-2")));
    assert_eq!(p.user_properties, vec![("conflict".to_owned(), "RequestWins".to_owned())]);
    task.abort();
}

/// `drop_connection` cuts the connection and the client reconnects without its subscriptions;
/// `restart_without_persistence` also forgets the retained messages; `events` lists it all in
/// order.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_connection_and_a_restart_without_persistence() {
    let broker = FakeBroker::start().await;
    let (client, events) = client(&broker, 10);
    let (task, _incoming) = pump_publishes(events);
    client.subscribe("s/#", QoS::AtLeastOnce).await.unwrap();
    client.publish("s/kept", QoS::AtLeastOnce, true, "v").await.unwrap();
    assert!(broker.wait_until(BOUND, |b| b.retained_on("s/kept").is_some()).await);
    broker.drop_connection();
    assert!(broker.wait_until(BOUND, |b| b.connections() == 2).await, "the client did not reconnect after the cut");
    assert!(broker.retained_on("s/kept").is_some(), "a dropped connection lost a retained message");
    broker.restart_without_persistence();
    assert!(broker.wait_until(BOUND, |b| b.connections() == 3).await, "the client did not reconnect after the restart");
    assert!(broker.retained().is_empty(), "a restart without persistence kept a retained message");
    let kinds: Vec<&str> = broker
        .events()
        .iter()
        .map(|e| match e {
            BrokerEvent::Connected => "connected",
            BrokerEvent::Subscribed(_) => "subscribed",
            BrokerEvent::Unsubscribed(_) => "unsubscribed",
            BrokerEvent::Published(_) => "published",
            BrokerEvent::Disconnected => "disconnected",
        })
        .collect();
    assert_eq!(kinds, ["connected", "subscribed", "published", "connected", "connected"]);
    task.abort();
}

/// Past its cap the broker stops listening: a further connection is refused by the operating
/// system, and the open one goes on.
#[tokio::test(flavor = "multi_thread")]
async fn the_broker_stops_listening_past_its_cap() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(1);
    let mut first = raw_client(&broker).await;
    let refused = tokio::time::timeout(BOUND, async {
        loop {
            match tokio::net::TcpStream::connect(broker.address()).await {
                Err(_) => return,
                Ok(mut extra) => {
                    // Accepted by the kernel before the listener closed: closed unanswered.
                    let mut buf = [0u8; 16];
                    let read = tokio::time::timeout(BOUND, extra.read(&mut buf)).await.expect("the extra connection is closed");
                    assert!(matches!(read, Ok(0) | Err(_)), "a connection past the cap was served");
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(refused.is_ok(), "the broker went on listening past its cap");
    assert!(broker.send("still", "open"), "the open connection was closed at the cap");
    let mut buf = [0u8; 64];
    let n = tokio::time::timeout(BOUND, first.read(&mut buf)).await.expect("the open connection still delivers").unwrap();
    assert!(n > 0);
}

/// A client's DISCONNECT is recorded — the short form rumqttc writes, with no reason code, which
/// rumqttc's own reader refuses.
#[tokio::test(flavor = "multi_thread")]
async fn a_clients_disconnect_is_recorded() {
    let broker = FakeBroker::start().await;
    let (client, events) = client(&broker, 10);
    let (task, _incoming) = pump_publishes(events);
    assert!(broker.wait_until(BOUND, |b| b.connections() == 1).await);
    client.disconnect().await.unwrap();
    assert!(
        broker.wait_until(BOUND, |b| b.events().iter().any(|e| matches!(e, BrokerEvent::Disconnected))).await,
        "the client's DISCONNECT was not recorded: {:?}",
        broker.events()
    );
    task.abort();
}

/// Another client's publish reaches the connected client only through a matching subscription,
/// at QoS 1, and keeps its retain flag only for a subscription made retain-as-published; a
/// retained one is kept, for later subscriptions (review: injected requests bypassed matching).
#[tokio::test(flavor = "multi_thread")]
async fn another_clients_publish_goes_through_the_subscriptions_at_qos_1() {
    use rumqttc::v5::mqttbytes::v5::Filter;
    let broker = FakeBroker::start().await;
    let (client, events) = client(&broker, 10);
    let (task, mut incoming) = pump_publishes(events);
    let preserving = Filter { preserve_retain: true, ..Filter::new("kept/+", QoS::AtLeastOnce) };
    client.subscribe_many(vec![preserving, Filter::new("plain/#", QoS::AtLeastOnce)]).await.unwrap();
    assert!(broker.wait_for_subscription("plain/#", BOUND).await);
    assert!(broker.publish("kept/a", "1", &SendOptions::default().retained()), "a matching publish was not delivered");
    let kept = next_publish(&mut incoming).await;
    assert_eq!((kept.qos, kept.retain), (QoS::AtLeastOnce, true), "retain-as-published was not honoured: {kept:?}");
    assert!(broker.publish("plain/b", "2", &SendOptions::default().retained()));
    let plain = next_publish(&mut incoming).await;
    assert_eq!((plain.qos, plain.retain), (QoS::AtLeastOnce, false), "a live publish was flagged retained: {plain:?}");
    assert!(!broker.publish("other/c", "3", &SendOptions::default()), "a publish nothing subscribed to was delivered");
    assert_eq!(broker.retained_on("kept/a").as_deref(), Some(&b"1"[..]), "a retained publish was not kept");
    task.abort();
}

/// A packet larger than the client's maximum packet size (its CONNECT) is discarded, never sent;
/// smaller ones go on (MQTT v5; review B3).
#[tokio::test(flavor = "multi_thread")]
async fn a_packet_past_the_clients_maximum_is_discarded() {
    let broker = FakeBroker::start().await;
    broker.retain("big/a", vec![b'x'; 4096], &SendOptions::default());
    broker.retain("big/b", "small", &SendOptions::default());
    let mut options = MqttOptions::new("small-client", "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(5)).set_max_packet_size(Some(1024));
    let (client, events) = AsyncClient::new(options, 10);
    let (task, mut incoming) = pump_publishes(events);
    client.subscribe("big/#", QoS::AtLeastOnce).await.unwrap();
    let first = next_publish(&mut incoming).await;
    assert_eq!(&first.payload[..], b"small", "the packet past the client's maximum was sent");
    assert_eq!(broker.connections(), 1, "the client was dropped");
    task.abort();
}

/// A new connection starts clean: a SUBACK held for an earlier connection, and the retained
/// messages its subscription replays, are not sent to the new one on release (the R1 re-gate's
/// minor: released after a reconnect, they reached a client that never asked for them).
#[tokio::test(flavor = "multi_thread")]
async fn held_subacks_do_not_outlive_their_connection() {
    let broker = FakeBroker::start().await;
    broker.retain("x/1", "old", &SendOptions::default());
    broker.hold_subacks();
    let (first, events) = client(&broker, 10);
    let (first_task, _) = pump(events);
    first.subscribe("x/#", QoS::AtLeastOnce).await.unwrap();
    assert!(broker.wait_for_subscription("x/#", BOUND).await, "the first client never subscribed");
    first_task.abort();
    broker.drop_connection();
    let mut options = MqttOptions::new("second-client", "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(5));
    let (_second, events) = AsyncClient::new(options, 10);
    let (_second_task, mut incoming) = pump(events);
    let deadline = std::time::Instant::now() + BOUND;
    while broker.connections() < 2 {
        assert!(std::time::Instant::now() < deadline, "the second client never connected");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    broker.release_subacks();
    let got = tokio::time::timeout(Duration::from_millis(500), incoming.recv()).await;
    assert!(got.is_err(), "a retained message held for an earlier connection reached the new one: {got:?}");
}

/// A denied filter gets `NotAuthorized` in its SUBACK, and no subscription: nothing is replayed.
#[tokio::test(flavor = "multi_thread")]
async fn a_denied_subscription_is_refused_and_replays_nothing() {
    let broker = FakeBroker::start().await;
    broker.retain("secret/1", "x", &SendOptions::default());
    broker.deny_subscriptions("secret/");
    let (client, events) = client(&broker, 10);
    let (_task, mut incoming) = pump(events);
    client.subscribe("secret/#", QoS::AtLeastOnce).await.unwrap();
    let got = tokio::time::timeout(Duration::from_millis(500), incoming.recv()).await;
    assert!(got.is_err(), "a denied subscription replayed: {got:?}");
    assert!(!broker.events().iter().any(|e| matches!(e, BrokerEvent::Subscribed(f) if f == "secret/#")), "a denied subscription was made");
}

/// The acks the client gets, by kind and packet id, in the order they come.
fn pump_acks(mut events: rumqttc::v5::EventLoop) -> (tokio::task::JoinHandle<()>, mpsc::UnboundedReceiver<(&'static str, u16)>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            match events.poll().await {
                Ok(Event::Incoming(rumqttc::v5::Incoming::SubAck(a))) => {
                    let _ = tx.send(("suback", a.pkid));
                }
                Ok(Event::Incoming(rumqttc::v5::Incoming::UnsubAck(a))) => {
                    let _ = tx.send(("unsuback", a.pkid));
                }
                Ok(Event::Incoming(rumqttc::v5::Incoming::PubAck(a))) => {
                    let _ = tx.send(("puback", a.pkid));
                }
                Ok(_) => {}
                Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
    });
    (task, rx)
}

/// An UNSUBSCRIBE answered while a SUBACK is held waits behind it, as a broker answers in order
/// (Claude's final gate minor 2: the UNSUBACK went first).
#[tokio::test(flavor = "multi_thread")]
async fn an_unsuback_waits_behind_a_held_suback() {
    let broker = FakeBroker::start().await;
    broker.hold_subacks();
    let (client, events) = client(&broker, 10);
    let (_task, mut acks) = pump_acks(events);
    client.subscribe("x/#", QoS::AtLeastOnce).await.unwrap();
    assert!(broker.wait_for_subscription("x/#", BOUND).await, "never subscribed");
    client.unsubscribe("x/#").await.unwrap();
    assert!(broker.wait_until(BOUND, |b| b.events().iter().any(|e| matches!(e, BrokerEvent::Unsubscribed(_)))).await, "never unsubscribed");
    assert!(tokio::time::timeout(Duration::from_millis(300), acks.recv()).await.is_err(), "an ack went out while the SUBACK was held");
    broker.release_subacks();
    let first = tokio::time::timeout(BOUND, acks.recv()).await.expect("no ack after the release");
    let second = tokio::time::timeout(BOUND, acks.recv()).await.expect("one ack after the release");
    assert_eq!((first.map(|a| a.0), second.map(|a| a.0)), (Some("suback"), Some("unsuback")), "the acks went out of order");
}

/// Delayed acks keep the order of their publishes, though the delay changes between them (Claude's
/// final gate minor 6: a task per ack let a shorter delay overtake a longer one).
#[tokio::test(flavor = "multi_thread")]
async fn delayed_acks_keep_the_order_of_their_publishes() {
    let broker = FakeBroker::start().await;
    let (client, events) = client(&broker, 10);
    let (_task, mut acks) = pump_acks(events);
    broker.delay_acks(Duration::from_millis(300));
    for i in 0..3u8 {
        client.publish("t", QoS::AtLeastOnce, false, vec![i]).await.unwrap();
    }
    assert!(broker.wait_until(BOUND, |b| b.received_on("t").len() == 3).await, "the first publishes never came");
    broker.delay_acks(Duration::from_millis(10));
    for i in 3..6u8 {
        client.publish("t", QoS::AtLeastOnce, false, vec![i]).await.unwrap();
    }
    assert!(broker.wait_until(BOUND, |b| b.received_on("t").len() == 6).await, "the next publishes never came");
    // No delay at all now: these must not overtake the delayed ones either (the confirming
    // review's minor).
    broker.delay_acks(Duration::ZERO);
    for i in 6..9u8 {
        client.publish("t", QoS::AtLeastOnce, false, vec![i]).await.unwrap();
    }
    let mut order = Vec::new();
    while order.len() < 9 {
        match tokio::time::timeout(BOUND, acks.recv()).await {
            Ok(Some(("puback", pkid))) => order.push(pkid),
            Ok(Some(_)) => {}
            _ => panic!("the acks stopped at {order:?}"),
        }
    }
    let mut sorted = order.clone();
    sorted.sort_unstable();
    assert_eq!(order, sorted, "delayed acks overtook one another");
}

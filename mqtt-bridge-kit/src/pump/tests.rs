//! The pump on its own: its overload flag, what it says when a session ends with requests unread,
//! the bounds on a broker that stalls; and the broker outage as one episode, across the sessions
//! of the MQTT side's loop (lifted from mqtt_ac's pump tests).

use std::sync::Arc;
use std::time::Duration;

use mqtt_test_broker::{FakeBroker, QoS};
use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::{AsyncClient, MqttOptions};

use super::*;
use crate::session::MqttTiming;
use crate::test_support::{Capture, Relay};

fn high(backlog: &Backlog) -> bool {
    backlog.high_since.load(Ordering::SeqCst) != 0
}

/// The owner's overload ruling (no-hang §14.6): the forward queue drops nothing; past HIGH_WATER
/// unread requests it raises its WARN once, and clears it back under LOW_WATER.
#[test]
fn a_backlog_past_high_water_is_flagged_once_and_cleared_when_it_drains() {
    let log = Capture::start();
    let backlog = Backlog::new();
    for _ in 0..HIGH_WATER - 1 {
        backlog.pushed();
    }
    assert!(!high(&backlog), "flagged below the high-water mark");
    backlog.pushed();
    assert!(high(&backlog), "not flagged at the high-water mark");
    backlog.pushed();
    assert_eq!(log.of_kind("mqtt_backlog_high").len(), 1, "flagged again while it stood");
    while backlog.depth.load(Ordering::SeqCst) > LOW_WATER + 1 {
        backlog.popped();
    }
    assert!(high(&backlog), "cleared above the low-water mark");
    backlog.popped();
    assert!(!high(&backlog), "not cleared at the low-water mark");
    assert_eq!(log.of_kind("mqtt_backlog_drained").len(), 1, "the drain was not said once");
}

/// A session that ends with requests still in the pump's queue drops them — the next session has a
/// queue of its own. One WARN says how many, and a standing high-water WARN is closed with its INFO.
#[test]
fn requests_left_unread_when_the_session_ends_are_reported() {
    let log = Capture::start();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let backlog = Arc::new(Backlog::new());
    for _ in 0..HIGH_WATER {
        backlog.pushed();
        let publish = Publish::new("t", QoS::AtLeastOnce, "x", None);
        assert!(tx.send(PumpEvent::Publish(publish, std::time::Instant::now())).is_ok());
    }
    drop(Incoming {
        rx,
        backlog,
        inflight: Arc::new(AtomicUsize::new(0)),
        written: Arc::new(AtomicU64::new(0)),
        progress: Arc::new(tokio::sync::Notify::new()),
        disconnect_asked: Arc::new(AtomicBool::new(false)),
    });
    let discarded = log.of_kind("mqtt_commands_discarded");
    assert!(
        discarded.len() == 1 && discarded[0].level == tracing::Level::WARN && discarded[0].field("discarded") == Some("1000"),
        "the requests left in the queue when the session ended were dropped silently; the log: {:#?}",
        log.records()
    );
    assert_eq!(log.of_kind("mqtt_backlog_drained").len(), 1, "the high-water WARN standing for the session's queue was never closed");
}

/// A broker outage is one episode: INFO `connection_lost` on its first failure, DEBUG retries,
/// one WARN `external_failure` past its threshold, INFO `external_recovered` once a connection has
/// held — and a broker that takes each connection and drops it at once is the same one outage,
/// never an INFO pair per connection (the fleet's F2: recovery on proof of life, never at CONNACK).
/// Each connection is a session of its own, so this is the MQTT side's whole loop, not one pump.
#[tokio::test]
async fn a_broker_outage_is_one_episode_recovered_only_on_a_connection_that_holds() {
    let log = Capture::start();
    let broker = FakeBroker::start().await;
    broker.cap_connections(80);
    let relay = Relay::start(broker.address()).await;
    let timing = MqttTiming {
        keep_alive: Duration::from_secs(5),
        retry: Duration::from_millis(50),
        poll_retry: Duration::from_millis(50),
        warn_after: Duration::from_millis(600),
        stable_after: Duration::from_millis(400),
    };
    let shared = shared_through(&relay, timing);
    let command = shared.topics.command();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mqtt = tokio::spawn(crate::session::run(shared, stopped));
    assert!(broker.wait_for_subscription(&command, Duration::from_secs(5)).await, "the MQTT side never subscribed");

    // The broker accepts each connection and drops it 100 ms later, for a second.
    relay.drop_connections_after(Some(Duration::from_millis(100)));
    relay.cut();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(relay.accepted() >= 3, "the broker was not reconnected to while it dropped every connection");
    assert_eq!(log.of_kind("connection_lost").len(), 1, "the outage began more than once; the log: {:#?}", log.records());
    assert!(log.of_kind("external_recovered").is_empty(), "a connection dropped at once ended the outage; the log: {:#?}", log.records());
    assert_eq!(log.of_kind("external_failure").len(), 1, "the outage past its threshold was not one WARN; the log: {:#?}", log.records());

    // Connections hold again: the outage ends once one has lasted, with its length.
    relay.drop_connections_after(None);
    // Something from the broker now and then: the proof of life the pump reads.
    let recovered = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let _ = broker.send("ping", "x");
            tokio::time::sleep(Duration::from_millis(100)).await;
            if !log.of_kind("external_recovered").is_empty() {
                return;
            }
        }
    })
    .await;
    assert!(recovered.is_ok(), "the outage never ended on a connection that held; the log: {:#?}", log.records());
    assert_eq!(log.of_kind("connection_lost").len(), 1);
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), mqtt).await;
}

/// A broker that takes each connection and drops it at once — two bridges sharing a client id — is
/// paced as one that refuses: a session shorter than `stable_after` counts as one more failure, so
/// the `retry` pause comes after five in a row (Claude's re-gate minor 12: every accepted
/// connection reset the count, and the bridge reconnected at `poll_retry` for good).
#[tokio::test]
async fn connections_dropped_at_once_are_paced_as_failures() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(80);
    let relay = Relay::start(broker.address()).await;
    let timing = MqttTiming {
        keep_alive: Duration::from_secs(5),
        retry: Duration::from_secs(3),
        poll_retry: Duration::from_millis(50),
        warn_after: Duration::from_secs(30),
        stable_after: Duration::from_millis(400),
    };
    let shared = shared_through(&relay, timing);
    let command = shared.topics.command();
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let mqtt = tokio::spawn(crate::session::run(shared, stopped));
    assert!(broker.wait_for_subscription(&command, Duration::from_secs(5)).await, "the MQTT side never subscribed");
    relay.drop_connections_after(Some(Duration::from_millis(100)));
    let before = relay.accepted();
    relay.cut();
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let made = relay.accepted() - before;
    assert!(made >= 3, "the broker was not reconnected to: {made} connections");
    assert!(made <= 6, "{made} connections in 2 s to a broker that dropped each at once: the retry pause never came");
    let _ = stop.send(true);
    let _ = tokio::time::timeout(Duration::from_secs(5), mqtt).await;
}

/// The MQTT side's shared state, for a broker reached through `relay`.
fn shared_through(relay: &Relay, timing: MqttTiming) -> Arc<crate::session::Shared> {
    let (host, port) = relay.address.rsplit_once(':').expect("the relay's host and port");
    Arc::new(crate::session::Shared {
        topics: crate::topics::Topics::new("T", "outage").expect("topics"),
        broker_host: host.to_owned(),
        broker_port: port.parse().expect("the relay's port"),
        timing,
        version: "test".into(),
        commands: Vec::new(),
        mailbox: crate::mailbox::Mailbox::new(),
        reports: crate::reports::Reports::new(),
        outage: Arc::new(BrokerOutage::new(timing.warn_after, timing.stable_after)),
        read_back_requested: AtomicBool::new(false),
        read_back_next: AtomicBool::new(false),
        enqueued: std::sync::atomic::AtomicU64::new(0),
    })
}

fn direct_client(broker: &FakeBroker, id: &str, capacity: usize) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let mut options = MqttOptions::new(id, "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(5));
    AsyncClient::new(options, capacity)
}

/// The pump's events until it ends, or `None` past `within`; the reason it gave.
async fn ended_within(incoming: &mut Incoming, within: Duration) -> Option<String> {
    tokio::time::timeout(within, async {
        loop {
            match incoming.recv().await {
                Some(PumpEvent::Ended(reason)) => return Some(reason),
                None => return Some("the queue closed".to_owned()),
                Some(_) => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// A broker that answers PINGREQ but withholds its PUBACKs makes no progress on what the client
/// asked, and rumqttc never notices: its pings are answered. With requests outstanding and nothing
/// acknowledged for twice the keep-alive, the pump ends the session, discarding the event loop, so
/// a fresh one is made (Bridge Runtime review C1).
#[tokio::test(flavor = "multi_thread")]
async fn a_broker_that_acknowledges_nothing_ends_the_session_within_twice_the_keep_alive() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let (client, events) = direct_client(&broker, "unacknowledged", 100);
    let outage = Arc::new(BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5)));
    let (_pump, mut incoming) = Pump::start(events, outage, stall_bound(Duration::from_secs(5)), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no first CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    broker.hold_acks();
    client.try_publish("t/x", QoS::AtLeastOnce, false, "x").expect("queue a publish");
    let started = std::time::Instant::now();
    let ended = ended_within(&mut incoming, Duration::from_secs(20)).await;
    let reason = ended.expect("the pump went on for 20 s with a publish its broker never acknowledged");
    assert!(reason.contains("acknowledged nothing"), "the session ended for another reason: {reason}");
    let took = started.elapsed();
    assert!(took >= Duration::from_secs(9), "ended sooner than twice the keep-alive: {took:?}");
    // Twice the keep-alive, and a stated tolerance for scheduling on a loaded machine.
    assert!(took <= Duration::from_secs(10) + STALL_TOLERANCE, "ended later than twice the keep-alive and {STALL_TOLERANCE:?}: {took:?}");
    assert!(broker.held_acks() >= 1, "nothing was held, so this test proves nothing");
}

/// How late past its bound a stalled session may end, for scheduling on a loaded machine.
const STALL_TOLERANCE: Duration = Duration::from_secs(2);

/// A broker that stops reading leaves rumqttc in its flush, inside `poll`, where its keep-alive
/// cannot fire; the kernel gives such a connection up only after many minutes. The pump bounds
/// the poll itself at twice the keep-alive and ends the session, discarding the event loop rather
/// than polling a cancelled one again (Bridge Runtime review C1).
#[tokio::test(flavor = "multi_thread")]
async fn a_poll_that_does_not_return_ends_the_session_within_twice_the_keep_alive() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    // A long keep-alive, so rumqttc's own ping check cannot end the session first: what ends it
    // must be the pump's bound on the poll (twice 5 s, given below).
    let mut options = MqttOptions::new("unread", "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(60));
    let (client, events) = AsyncClient::new(options, 128);
    let outage = Arc::new(BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5)));
    let (_pump, mut incoming) = Pump::start(events, outage, stall_bound(Duration::from_secs(5)), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no first CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    broker.stop_reading();
    // Far more than the two sockets' buffers hold, at QoS 0 (no in-flight limit): the flush blocks.
    let payload = vec![0u8; 1 << 20];
    let started = std::time::Instant::now();
    for _ in 0..64 {
        client.try_publish("big", QoS::AtMostOnce, false, payload.clone()).expect("queue a publish");
    }
    let ended = ended_within(&mut incoming, Duration::from_secs(20)).await;
    let reason = ended.expect("the pump's poll did not return for 20 s with the broker not reading");
    assert!(reason.contains("did not return"), "the session ended for another reason: {reason}");
    let took = started.elapsed();
    assert!(took <= Duration::from_secs(10) + STALL_TOLERANCE, "ended later than twice the keep-alive and {STALL_TOLERANCE:?}: {took:?}");
    // Bytes were still in the flush when the bound fired: an idle poll — nothing to write — would
    // end the same way with this long keep-alive (the R1 re-gate's minor).
    let delivered = broker.received_on("big").len();
    assert!(
        delivered < 64,
        "every publish reached the broker: the flush was never blocked, so this test proves nothing ({delivered} of 64)"
    );
}

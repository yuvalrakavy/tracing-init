//! The pump on its own: its overload flag, what it says when a session ends with requests unread,
//! the event loop it keeps alive across a reconnect, and the broker outage as one episode (lifted
//! from mqtt_ac's pump tests).

use std::sync::Arc;
use std::time::Duration;

use mqtt_test_broker::{FakeBroker, QoS};
use rumqttc::v5::mqttbytes::v5::Publish;
use rumqttc::v5::{AsyncClient, MqttOptions};

use super::*;
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

fn client_through(relay: &Relay, id: &str, keep_alive: Duration) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let (host, port) = relay.address.rsplit_once(':').expect("the relay's host and port");
    let mut options = MqttOptions::new(id, host, port.parse::<u16>().expect("the relay's port"));
    options.set_keep_alive(keep_alive);
    AsyncClient::new(options, 10)
}

/// rumqttc 0.25 stops taking requests while a packet-id collision stands, until that id's PUBACK
/// arrives. A connection lost meanwhile is cleaned up without the collision, so that PUBACK never
/// comes, and the event loop never takes another request. The pump moves the collision back into
/// `pending` (mqtt_ac's fix).
#[tokio::test(flavor = "multi_thread")]
async fn a_collision_standing_when_the_connection_drops_does_not_stop_the_event_loop_for_good() {
    let broker = FakeBroker::start_with_receive_max(2).await;
    broker.cap_connections(50);
    let relay = Relay::start(broker.address()).await;
    let (client, events) = client_through(&relay, "collision", Duration::from_secs(5));
    // Queued before the first poll: `a` takes packet id 1 (its ack held), the subscription 2, and
    // `b` wraps to 1 — a collision, standing while `a`'s ack is held.
    broker.hold_acks();
    client.try_publish("t/a", QoS::AtLeastOnce, false, "a").expect("queue a");
    client.try_subscribe("t/s", QoS::AtLeastOnce).expect("queue the subscription");
    client.try_publish("t/b", QoS::AtLeastOnce, false, "b").expect("queue b");
    let outage = Arc::new(BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5)));
    let (_pump, _incoming) = Pump::start(events, outage, Duration::from_millis(100));
    assert!(broker.wait_for_subscription("t/s", Duration::from_secs(10)).await, "the client never subscribed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(broker.received_on("t/b").is_empty(), "`b` was sent, so no collision stood and this test proves nothing");

    relay.cut();
    assert!(broker.wait_until(Duration::from_secs(5), |b| b.connections() >= 2).await, "the pump never reconnected");
    broker.release_acks();
    client.try_publish("t/c", QoS::AtLeastOnce, false, "c").expect("queue c");
    let sent = broker.wait_until(Duration::from_secs(10), |b| !b.received_on("t/c").is_empty()).await;
    assert!(
        sent,
        "nothing was sent after the reconnect: the collision standing at the drop outlived it, and the event loop takes no request while one stands"
    );
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
        assert!(tx.send(PumpEvent::Publish(publish)).is_ok());
    }
    drop(Incoming { rx, backlog });
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
#[tokio::test]
async fn a_broker_outage_is_one_episode_recovered_only_on_a_connection_that_holds() {
    let log = Capture::start();
    let broker = FakeBroker::start().await;
    broker.cap_connections(80);
    let relay = Relay::start(broker.address()).await;
    let (_client, events) = client_through(&relay, "outage", Duration::from_secs(5));
    let outage = Arc::new(BrokerOutage::new(Duration::from_millis(600), Duration::from_millis(400)));
    let (_pump, mut incoming) = Pump::start(events, outage, Duration::from_millis(50));
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no first CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));

    // The broker accepts each connection and drops it 100 ms later, for a second.
    relay.drop_connections_after(Some(Duration::from_millis(100)));
    relay.cut();
    let reconnects = tokio::time::timeout(Duration::from_millis(1200), async {
        let mut connacks = 0;
        loop {
            if let Some(PumpEvent::Connected) = incoming.recv().await {
                connacks += 1;
            }
            if connacks >= 1000 {
                return connacks;
            }
        }
    })
    .await;
    assert!(reconnects.is_err(), "the pump stopped reconnecting");
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
            let _ = tokio::time::timeout(Duration::from_millis(100), incoming.recv()).await;
            if !log.of_kind("external_recovered").is_empty() {
                return;
            }
        }
    })
    .await;
    assert!(recovered.is_ok(), "the outage never ended on a connection that held; the log: {:#?}", log.records());
    assert_eq!(log.of_kind("connection_lost").len(), 1);
}

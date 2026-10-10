//! Packet ids (MQTT §2.2.1: an id is not used again until its acknowledgement) for a fixed bridge:
//! the host's packet-id budget (multi-device spec §4.3) covers the one fixed instance too, since a
//! `Bridge` is the host's N = 1 mode.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use serde_json::json;

/// The outer bound on the scenario.
const DEADLINE: Duration = Duration::from_secs(60);

/// While a SUBACK of the bridge's is outstanding, no packet id it holds is sent again: rumqttc
/// draws SUBSCRIBE and publish ids from one counter wrapping at the broker's receive maximum (4
/// here) and checks only its publishes, so `Active`, `Version` and the retained model published
/// right behind the subscriptions took the id of the `Command` SUBSCRIBE still awaiting its SUBACK.
/// With the budget, publication waits for it; once it comes, everything goes out.
#[tokio::test(flavor = "multi_thread")]
async fn no_packet_id_is_reused_while_a_suback_is_outstanding() {
    let scenario = async {
        let broker = FakeBroker::start_with_receive_max(4).await;
        let h = Harness::start_unwaited_on(broker, "pkids-fixed", acknowledged(), quick_timing(), |broker, topics, device| {
            // The last request subscription's SUBACK is held: its packet id stays in use.
            broker.hold_subacks_matching(&topics.command());
            for t in 0..8 {
                device.set(&format!("t{t}"), "n", json!(t));
            }
        })
        .await;
        let t = h.topics.clone();
        assert!(h.broker.wait_for_subscription(&t.command(), bound()).await, "the bridge never subscribed to its commands");
        assert!(
            h.eventually(bound(), |h| h.device.connected() && !h.device.calls_of(Op::Read).is_empty()).await,
            "the device never connected and reported, so this test proves nothing"
        );
        // Long enough for every message the bridge had to go out, were it not held.
        h.settle(Duration::from_millis(1500)).await;
        let reuses = h.broker.pkid_reuses();
        assert!(reuses.is_empty(), "a packet id was reused while its SUBACK was outstanding: {reuses:#?}");
        h.broker.release_subacks();
        assert!(
            h.eventually(bound(), |h| h.status().as_deref() == Some("connected") && (0..8).all(|n| h.state(&format!("t{n}")).is_some()))
                .await,
            "the model was not published once the SUBACK came: status {:?}",
            h.status()
        );
        assert_eq!(h.broker.retained_on(&t.active()).as_deref(), Some(&b"true"[..]), "the bridge never said Active=true");
        let reuses = h.broker.pkid_reuses();
        assert!(reuses.is_empty(), "a packet id was reused while its SUBACK was outstanding: {reuses:#?}");
        assert!(h.stop().await);
    };
    tokio::time::timeout(DEADLINE, scenario).await.expect("the scenario did not end within its deadline");
}

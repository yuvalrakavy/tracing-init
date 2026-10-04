//! Reports never wait (Bridge Runtime spec §4.2, §5; mqtt_ac re-review C1): with the broker
//! stalled — its acknowledgements held, the client's request channel full, the session parked on a
//! publish — the device side still does its work.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use serde_json::json;

/// A request held while the device was down is applied when the device returns, though the broker
/// is stalled throughout the catch-up (which reports the device's status and state).
#[tokio::test(flavor = "multi_thread")]
async fn the_device_side_works_while_the_broker_is_stalled() {
    let h = Harness::start_with("stalled", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set_up(false);
    })
    .await;
    assert!(h.desire("t1", "temp", json!(22), Req { expiry: Some(60), conflict: Some("RequestWins") }));
    h.settle(Duration::from_millis(300)).await;

    // Stall the broker, and fill the session's path to it: each malformed command is an Error to
    // publish, and past the in-flight limit and the request channel the session waits.
    h.broker.hold_acks();
    for i in 0..300 {
        assert!(h.send_raw(&h.topics.command(), format!("not json {i}").as_bytes()));
    }
    assert!(
        h.eventually(BOUND, |h| h.broker.held_acks() >= 10).await,
        "the broker's acknowledgements were never held: the path is not saturated"
    );
    h.settle(Duration::from_millis(300)).await;

    h.device.set_up(true);
    assert!(
        h.eventually(Duration::from_secs(5), |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "with the broker stalled the device worker did not apply the held request: it waits on MQTT"
    );
    h.broker.release_acks();
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await, "the bridge did not recover with the broker");
    assert!(h.stop().await);
}

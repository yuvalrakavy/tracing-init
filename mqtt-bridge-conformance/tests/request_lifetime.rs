//! Public builders carry the optional policy into both production device paths.
use mqtt_bridge_conformance::{
    host::{Fleet, HostHarness},
    *,
};
use mqtt_bridge_kit::{topics::Topics, Bridge};
use serde_json::{json, Value};
use std::{sync::Arc, time::Duration};

#[tokio::test(flavor = "multi_thread")]
async fn request_lifetime_public_fixed_builder_requires_bounded_scalar_expiry() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(10);
    let name = instance_name("limited-fixed");
    let topics = Topics::new(ROOT, &name).unwrap();
    let (driver, device) = fake_device(acknowledged());
    device.set("t1", "temp", json!(20));
    let running = Bridge::new(ROOT)
        .instance(&name)
        .broker(&broker.address())
        .timing(quick_timing())
        .mqtt_timing(quick_mqtt())
        .request_lifetime_limit(Duration::from_secs(180))
        .start(driver)
        .unwrap();
    assert!(broker.wait_for_subscription(&topics.command(), bound()).await);
    let topic = topics.desired("t1", "temp");
    assert!(broker.publish(&topic, "22", &SendOptions::default().retained()));
    assert!(
        broker
            .wait_until(bound(), |b| b
                .received_on(&topics.error())
                .iter()
                .any(|m| { serde_json::from_slice::<Value>(&m.payload).is_ok_and(|v| v["reason"] == "malformed") }))
            .await,
        "fixed builder lost its expiry policy"
    );
    assert!(device.applies().is_empty());
    let options = SendOptions { message_expiry: Some(60), ..SendOptions::default() }.retained();
    assert!(broker.publish(&topic, "22", &options));
    assert!(broker.wait_until(bound(), |_| device.get("t1", "temp") == Some(json!(22))).await);
    assert!(tokio::time::timeout(bound(), running.stop()).await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn request_lifetime_public_host_builder_reaches_factory_created_devices() {
    let broker = Arc::new(FakeBroker::start_multi().await);
    broker.cap_connections(10);
    let fleet = Fleet::new(acknowledged());
    fleet.prepare("unit", |d| d.set("t1", "temp", json!(20)));
    let h = HostHarness::join(broker, &instance_name("limited-host"), fleet, |host| host.request_lifetime_limit(Duration::from_secs(180)))
        .await;
    assert!(h.configure("unit", json!({})), "configuration needs no expiry");
    assert!(h.eventually(bound(), |h| h.status("unit").as_deref() == Some("connected")).await);
    assert!(h.desire("unit", "t1", "temp", json!(22), Req::default()));
    assert!(h.eventually(bound(), |h| !h.errors_for("unit", "malformed").is_empty()).await, "configured device lost its expiry policy");
    assert!(h.fleet.device("unit").unwrap().applies().is_empty());
    assert!(h.desire("unit", "t1", "temp", json!(22), Req { expiry: Some(60), conflict: None }));
    assert!(h.eventually(bound(), |h| h.fleet.device("unit").unwrap().get("t1", "temp") == Some(json!(22))).await);
    assert!(h.stop().await);
}

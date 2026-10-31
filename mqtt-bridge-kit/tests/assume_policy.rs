//! Exercise builder policy through the actual MQTT pump, not only session fixtures.
use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
use std::time::Duration;
use mqtt_bridge_kit::{AssumePolicy, Bridge, Host, CommandCall, DeviceDriver, DriverInfo, LinkContext, LinkError, OpError, PropertyMap, Scope, TargetState};
use mqtt_bridge_kit::topics::Topics;
use mqtt_test_broker::{FakeBroker, SendOptions};
use serde_json::{json, Value};

struct MetadataOnly { actuator_calls: Arc<AtomicUsize> }
impl DeviceDriver for MetadataOnly {
    fn info(&self) -> DriverInfo { DriverInfo::default() }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> { Ok(()) }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![TargetState::new("observed", json!({"closed":false}).as_object().unwrap().clone())])
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> { Ok(()) }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> { self.actuator_calls.fetch_add(1,Ordering::SeqCst);Ok(()) }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> { self.actuator_calls.fetch_add(1,Ordering::SeqCst);Ok(None) }
}
async fn builder_restriction(dynamic: bool) {
    let broker = FakeBroker::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let topics = Topics::new("AssumeProbe","i").unwrap();
    let running = if dynamic {
        let actuator_calls = calls.clone();
        let running = Host::new("AssumeProbe").broker(&broker.address())
            .assume_policy(AssumePolicy::ConfiguredAssumed)
            .device(move |_| Ok(MetadataOnly { actuator_calls:actuator_calls.clone() }))
            .start().unwrap();
        broker.publish("AssumeProbe/Config/i",json!({"bridge":"AssumeProbe"}).to_string(),&SendOptions::default().retained());
        running
    } else {
        Bridge::new("AssumeProbe").instance("i").broker(&broker.address())
            .assume_policy(AssumePolicy::ConfiguredAssumed)
            .start(MetadataOnly { actuator_calls:calls.clone() }).unwrap()
    };
    assert!(broker.wait_until(Duration::from_secs(3), |b| b.received_on(&topics.status()).iter().any(|p| p.payload.as_ref()==b"connected")).await);
    assert!(broker.wait_for_subscription(&topics.command(),Duration::from_secs(3)).await,"command subscription missing");
    let opts = SendOptions::default().expiry(30);
    broker.publish(&topics.command(),json!({"target":"observed","command":"assume","closed":true}).to_string(),&opts);
    assert!(broker.wait_until(Duration::from_secs(3), |b| b.received_on(&topics.error()).iter().any(|p| serde_json::from_slice::<Value>(&p.payload).is_ok_and(|v| v["reason"]=="refused"))).await,"opt-in policy did not refuse observed-target assume");
    let seen = broker.received_on(&topics.state("observed"));
    assert!(seen.iter().all(|p| serde_json::from_slice::<Value>(&p.payload).unwrap()["closed"]==false));
    broker.publish(&topics.config("cco"),json!({"feedback":"assumed"}).to_string(),&SendOptions::default().retained());
    // Metadata-only repetition also handles the asynchronous Config-delivery boundary.
    let admitted = tokio::time::timeout(Duration::from_secs(3),async {
        loop {
            broker.publish(&topics.command(),json!({"target":"cco","command":"assume","closed":true}).to_string(),&opts);
            if broker.received_on(&topics.state("cco")).iter().any(|p| serde_json::from_slice::<Value>(&p.payload).is_ok_and(|v| v["closed"]==true && v["assumed"]==true)) { break; }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }).await;
    assert!(admitted.is_ok(),"accepted CCO assumption did not reach State");
    assert_eq!(calls.load(Ordering::SeqCst),0,"assume invoked physical actuator operations");
    assert!(running.stop().await);
}

#[tokio::test]
async fn fixed_builder_restriction_reaches_mqtt_without_actuator_calls() { builder_restriction(false).await; }
#[tokio::test]
async fn dynamic_builder_restriction_reaches_mqtt_without_actuator_calls() { builder_restriction(true).await; }

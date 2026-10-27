use crate::*;
use mqtt_test_broker::{FakeBroker, SendOptions};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

struct Driver(Arc<AtomicUsize>);
impl DeviceDriver for Driver {
    fn info(&self) -> DriverInfo {
        DriverInfo { commands: vec![CommandSpec::new("pulse", CommandClass::Momentary)], ..Default::default() }
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
}

#[tokio::test]
async fn launched_strict_bridge_rejects_duplicates_with_correlated_errors_and_keeps_legacy_opt_in() {
    for strict in [false, true] {
        let broker = FakeBroker::start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let bridge = Bridge::new("Strict").instance("i").broker(&broker.address());
        let bridge = if strict { bridge.strict_json() } else { bridge };
        let running = bridge.start(Driver(calls.clone())).unwrap();
        assert!(
            broker
                .wait_until(Duration::from_secs(2), |broker| broker
                    .received_on("Strict/Status/i")
                    .iter()
                    .any(|message| message.payload.as_ref() == b"connected"))
                .await,
            "fixture never established a healthy worker"
        );
        let request_id = "a".repeat(64);
        let options = SendOptions {
            user_properties: vec![("request-id".into(), request_id.clone())],
            response_topic: Some("Client/replies".into()),
            correlation_data: Some(bytes::Bytes::from_static(b"original-correlation")),
            ..Default::default()
        };
        for (topic, payload) in [
            ("Strict/Config/i/u1", r#"{"level":1,"level":2}"#),
            ("Strict/Desired/i/u1", r#"{"level":1,"level":2}"#),
            ("Strict/Command/i", r#"{"command":"pulse","command":"pulse"}"#),
        ] {
            assert!(broker.send_with(topic, payload, &options));
        }
        if strict {
            assert!(
                broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Strict/Error/i").len() == 3).await,
                "strict flag failed to reach an ordinary decoder"
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0, "duplicate request reached the driver");
            for message in broker.received_on("Strict/Error/i") {
                let error: Value = serde_json::from_slice(&message.payload).unwrap();
                assert_eq!(error["request-id"], request_id, "strict refusal lost original request id");
                assert!(error["error"].as_str().unwrap().contains("duplicate JSON object key"));
            }
            assert!(
                broker.wait_until(Duration::from_secs(2), |broker| !broker.received_on("Client/replies").is_empty()).await,
                "duplicate command did not receive a correlated refusal"
            );
            let replies = broker.received_on("Client/replies");
            assert_eq!(replies[0].properties.as_ref().unwrap().correlation_data.as_deref(), Some(b"original-correlation".as_slice()));
            for (topic, payload) in [
                ("Strict/Config/i/u1", r#"{"level":3}"#),
                ("Strict/Desired/i/u1/level", "3"),
                ("Strict/Command/i", r#"{"command":"pulse"}"#),
            ] {
                assert!(broker.send_with(topic, payload, &options));
            }
        }
        assert!(
            broker.wait_until(Duration::from_secs(2), |_| calls.load(Ordering::SeqCst) == 3).await,
            "valid input or legacy duplicate-key behavior stopped working"
        );
        assert_eq!(broker.accepted(), 1, "strict refusal unnecessarily reconnected MQTT");
        assert!(running.stop().await);
    }
}

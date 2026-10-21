use super::*;
use crate::wire::{request_context, request_meta, Conflict};
use rumqttc::v5::mqttbytes::v5::PublishProperties;
use std::time::{Duration, Instant};

fn properties(entries: &[(&str, &str)]) -> PublishProperties {
    PublishProperties { user_properties: entries.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect(), ..PublishProperties::default() }
}

#[test]
fn diagnostic_traces_never_change_generation_authority_or_coalescing_keys() {
    use std::{
        collections::{BTreeSet, HashSet},
        hash::{Hash, Hasher},
    };
    let first = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    let second = "00-222233445566778899aabbccddeeff00-1234567890abcdef-01";
    let plain = RequestContext::new("epoch.7").unwrap();
    let a = plain.clone().with_traceparent(Some(first));
    let b = plain.clone().with_traceparent(Some(second));
    assert_eq!(a, b, "trace metadata changed mapping authority");
    assert_eq!(a, plain);
    assert_eq!(BTreeSet::from([a.clone(), b.clone(), plain.clone()]).len(), 1);
    assert_eq!(HashSet::from([a.clone(), b.clone(), plain]).len(), 1);
    let hash = |context: &RequestContext| {
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        context.hash(&mut hash);
        hash.finish()
    };
    assert_eq!(hash(&a), hash(&b));
    assert_eq!(a.traceparent(), Some(first));
    assert_eq!(b.traceparent(), Some(second));
    assert_ne!(a, RequestContext::new("epoch.8").unwrap());
}
#[test]
fn wire_trace_capture_is_bounded_optional_and_independent_of_generation_parsing() {
    let trace = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    for generation in [None, Some("epoch.7")] {
        let mut values = vec![("traceparent", trace)];
        if let Some(generation) = generation {
            values.push(("target-generation", generation));
        }
        let context = request_context(Some(&properties(&values))).unwrap();
        assert_eq!(context.traceparent(), Some(trace));
    }
    for invalid in ["garbage".to_owned(), "x".repeat(513), "00-00000000000000000000000000000000-1234567890abcdef-01".into()] {
        let context = request_context(Some(&properties(&[("traceparent", &invalid)]))).unwrap();
        assert_eq!(context.traceparent(), None);
        assert!(context.trace_was_captured());
    }
    let duplicate = properties(&[("traceparent", trace), ("traceparent", trace)]);
    assert_eq!(request_context(Some(&duplicate)).unwrap().traceparent(), None);
    let malformed = properties(&[("traceparent", trace), ("target-generation", "")]);
    assert!(request_context(Some(&malformed)).is_err());
    assert_eq!(crate::wire::trace_context(Some(&malformed)).traceparent(), Some(trace));
}

#[test]
fn opaque_context_preserves_the_literal_generation_without_inheriting_lane_policy() {
    assert_eq!(request_context(None).unwrap(), RequestContext::default());
    let props = properties(&[
        ("target-generation", "0123456789abcdef.37"),
        ("traceparent", "opaque unrelated metadata"),
        ("conflict", "not a Desired conflict"),
    ]);
    let context = request_context(Some(&props)).unwrap();
    assert_eq!(context.generation(), Some("0123456789abcdef.37"), "generation metadata was discarded");
    assert!(request_meta(Some(&props), Instant::now()).is_err(), "Desired conflict validation changed");
    let mut props = properties(&[("target-generation", "x.7"), ("conflict", "RequestWins")]);
    props.message_expiry_interval = Some(30);
    let now = Instant::now();
    let meta = request_meta(Some(&props), now).unwrap();
    assert_eq!(meta.context.generation(), Some("x.7"));
    assert_eq!(meta.deadline, Some(now + Duration::from_secs(30)));
    assert_eq!(meta.conflict, Conflict::RequestWins);
}

#[test]
fn invalid_or_duplicate_generation_properties_are_rejected_even_when_the_values_agree() {
    for value in ["".into(), " ".into(), "v 1".into(), "\n".into(), "\u{7f}".into(), "💡".into(), "x".repeat(129)] {
        assert!(request_context(Some(&properties(&[("target-generation", &value)]))).is_err(), "accepted malformed generation {value:?}");
        assert!(RequestContext::new(value).is_err());
    }
    for value in ["!".into(), "~".into(), "x".repeat(128)] {
        assert_eq!(request_context(Some(&properties(&[("target-generation", &value)]))).unwrap().generation(), Some(value.as_str()));
    }
    for second in ["x.1", "x.2"] {
        assert!(
            request_context(Some(&properties(&[("target-generation", "x.1"), ("target-generation", second)]))).is_err(),
            "accepted duplicate generation property"
        );
    }
    assert!(request_context(Some(&properties(&[("traceparent", "a"), ("traceparent", "b")]))).is_ok());
}

#[test]
fn fixed_policy_requires_valid_generations_and_factory_fencing_is_explicitly_unsupported() {
    let table = TargetGenerations {
        instance: "epoch.1".into(),
        targets: BTreeMap::from([("__inventory".into(), "epoch.1".into()), ("Bus1.Outputs.Kitchen".into(), "epoch.7".into())]),
    };
    assert!(RequestPolicy::Unfenced.validate(false).is_ok());
    assert!(RequestPolicy::Fenced(table.clone()).validate(true).is_ok());
    assert!(RequestPolicy::Fenced(table.clone()).validate(false).is_err(), "factory policy was silently accepted or downgraded");
    for case in 0..3 {
        let mut invalid = table.clone();
        match case {
            0 => invalid.instance = " ".into(),
            1 => {
                invalid.targets.insert("bad/target".into(), "epoch.7".into());
            }
            _ => {
                invalid.targets.insert("Kitchen".into(), "x".repeat(129));
            }
        }
        assert!(RequestPolicy::Fenced(invalid).validate(true).is_err(), "invalid generation table admitted");
    }
}

use crate::{DeviceDriver, DriverInfo, LinkContext, LinkError, OpError, Scope};

#[derive(Default)]
struct Legacy {
    calls: Vec<&'static str>,
}
impl DeviceDriver for Legacy {
    fn info(&self) -> DriverInfo {
        DriverInfo::default()
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        self.calls.push("read");
        Ok(vec![TargetState::new("Kitchen", PropertyMap::new())])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        self.calls.push("apply");
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        self.calls.push("execute");
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        self.calls.push("configure");
        Ok(())
    }
}

#[tokio::test]
async fn provided_context_operations_preserve_legacy_drivers_without_inventing_observation_tokens() {
    let mut driver = Legacy::default();
    let context = RequestContext::new("epoch.7").unwrap();
    let values = PropertyMap::new();
    let call = CommandCall { command: "pulse".into(), params: values.clone() };
    assert_eq!(driver.request_policy(), RequestPolicy::Unfenced);
    driver.validate_request(Some("Kitchen"), &context, RequestKind::Apply(&values)).unwrap();
    driver.apply_with_context("Kitchen", &values, &context).await.unwrap();
    driver.execute_with_context(Some("Kitchen"), &call, &context).await.unwrap();
    driver.configure_with_context("Kitchen", None, &context).await.unwrap();
    let states = driver.read_state_with_context(Scope::All).await.unwrap();
    assert_eq!(states[0].context, RequestContext::default(), "legacy read was stamped with a token it never carried");
    driver.probe().await.unwrap();
    assert_eq!(driver.calls, vec!["apply", "execute", "configure", "read", "read"]);
}

struct ContextOnly;
impl DeviceDriver for ContextOnly {
    fn info(&self) -> DriverInfo {
        DriverInfo::default()
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Err(OpError::Unsupported("uses contextual reads".into()))
    }
    async fn read_state_with_context(&mut self, scope: Scope) -> Result<Vec<ContextualState>, OpError> {
        assert_eq!(scope, Scope::Any);
        Ok(vec![ContextualState::new(TargetState::new("Kitchen", PropertyMap::new()), RequestContext::new("epoch.7").unwrap())])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

#[tokio::test]
async fn default_probe_uses_contextual_reads_for_a_driver_without_legacy_reads() {
    assert!(ContextOnly.probe().await.is_ok(), "default probe ignored contextual-only read support");
}

use mqtt_test_broker::{FakeBroker, SendOptions};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};

struct FencedDriver {
    connected: Arc<AtomicBool>,
    described: Arc<AtomicUsize>,
    correctly_described: Arc<AtomicBool>,
}
impl DeviceDriver for FencedDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo::default()
    }
    fn request_policy(&self) -> RequestPolicy {
        self.described.fetch_add(1, Ordering::SeqCst);
        RequestPolicy::Fenced(TargetGenerations {
            instance: "epoch.1".into(),
            targets: BTreeMap::from([("__inventory".into(), "epoch.1".into())]),
        })
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        self.correctly_described.store(self.described.load(Ordering::SeqCst) == 1, Ordering::SeqCst);
        self.connected.store(true, Ordering::SeqCst);
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}
fn fenced_driver() -> FencedDriver {
    FencedDriver {
        connected: Arc::new(AtomicBool::new(false)),
        described: Arc::new(AtomicUsize::new(0)),
        correctly_described: Arc::new(AtomicBool::new(false)),
    }
}

#[tokio::test]
async fn fixed_description_captures_policy_once_before_connect() {
    let broker = FakeBroker::start().await;
    let driver = fenced_driver();
    let (connected, described, correct) = (driver.connected.clone(), driver.described.clone(), driver.correctly_described.clone());
    let running = crate::Bridge::new("FenceTest").instance("Controller").broker(&broker.address()).start(driver).unwrap();
    let reached = broker.wait_until(Duration::from_secs(2), |_| connected.load(Ordering::SeqCst)).await;
    assert!(running.stop().await);
    assert!(reached);
    assert!(correct.load(Ordering::SeqCst), "fixed driver connected before policy capture");
    assert_eq!(described.load(Ordering::SeqCst), 1, "policy was described more than once");
}

#[tokio::test]
async fn a_factory_fenced_driver_is_rejected_before_its_connect_can_run() {
    let broker = FakeBroker::start().await;
    let connected = Arc::new(AtomicBool::new(false));
    let observed = connected.clone();
    broker.retain("FenceTest/Config/Controller", r#"{"bridge":"FenceTest"}"#, &SendOptions::default().retained());
    let running = crate::Host::new("FenceTest")
        .broker(&broker.address())
        .device(move |_| {
            let mut driver = fenced_driver();
            driver.connected = observed.clone();
            Ok(driver)
        })
        .start()
        .unwrap();
    let answered = broker
        .wait_until(Duration::from_secs(2), |b| {
            connected.load(Ordering::SeqCst)
                || b.received_on("FenceTest/Error/Controller")
                    .iter()
                    .any(|r| String::from_utf8_lossy(&r.payload).contains("generation fencing requires a fixed Bridge"))
        })
        .await;
    let error = broker
        .received_on("FenceTest/Error/Controller")
        .iter()
        .any(|r| String::from_utf8_lossy(&r.payload).contains("generation fencing requires a fixed Bridge"));
    assert!(running.stop().await);
    assert!(answered);
    assert!(!connected.load(Ordering::SeqCst), "unsupported factory fencing connected a driver");
    assert!(error, "unsupported factory fencing was not explicitly reported");
}

struct ConfigRecording {
    contexts: Arc<std::sync::Mutex<Vec<RequestContext>>>,
}
impl DeviceDriver for ConfigRecording {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            commands: vec![
                crate::CommandSpec::new("pulse", crate::CommandClass::Momentary),
                crate::CommandSpec::new("status", crate::CommandClass::Read),
            ],
            ..DriverInfo::default()
        }
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        self.contexts.lock().unwrap().push(RequestContext::default());
        Ok(None)
    }
    async fn execute_with_context(&mut self, _: Option<&str>, _: &CommandCall, context: &RequestContext) -> Result<Option<Value>, OpError> {
        self.contexts.lock().unwrap().push(context.clone());
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        self.contexts.lock().unwrap().push(RequestContext::default());
        Ok(())
    }
    async fn configure_with_context(&mut self, _: &str, _: Option<&Value>, context: &RequestContext) -> Result<(), OpError> {
        self.contexts.lock().unwrap().push(context.clone());
        Ok(())
    }
}

#[tokio::test]
async fn config_context_reaches_the_driver_and_equal_data_from_another_generation_is_distinct() {
    let broker = FakeBroker::start().await;
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let running = crate::Bridge::new("FenceTest")
        .instance("Controller")
        .broker(&broker.address())
        .start(ConfigRecording { contexts: contexts.clone() })
        .unwrap();
    assert!(broker.wait_for_subscription("FenceTest/Config/Controller/#", Duration::from_secs(2)).await);
    for (index, (generation, mode)) in [("epoch.1", "A"), ("epoch.2", "A"), ("epoch.2", "B"), ("epoch.1", "A")].into_iter().enumerate() {
        assert!(broker.send_with(
            "FenceTest/Config/Controller/Kitchen",
            serde_json::json!({"mode":mode}).to_string(),
            &SendOptions::default().user_property("target-generation", generation)
        ));
        let reached = broker.wait_until(Duration::from_secs(2), |_| contexts.lock().unwrap().len() > index).await;
        if !reached {
            assert!(running.stop().await);
            panic!("config from a fresh generation was deduplicated using old context");
        }
        let original = contexts.lock().unwrap()[index].generation().map(str::to_owned);
        if original.as_deref() != Some(generation) {
            assert!(running.stop().await);
            panic!("configuration lost its original generation context");
        }
    }
    assert!(running.stop().await);
}

#[tokio::test]
async fn command_context_survives_momentary_and_read_queues_and_duplicates_never_execute() {
    let broker = FakeBroker::start().await;
    let contexts = Arc::new(std::sync::Mutex::new(Vec::new()));
    let running = crate::Bridge::new("FenceTest")
        .instance("Controller")
        .broker(&broker.address())
        .start(ConfigRecording { contexts: contexts.clone() })
        .unwrap();
    assert!(broker.wait_for_subscription("FenceTest/Command/Controller", Duration::from_secs(2)).await);
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |b| b.received_on("FenceTest/Status/Controller").iter().any(|r| r.payload == "connected"))
            .await
    );
    for (index, (command, generation)) in [("pulse", "epoch.1"), ("status", "epoch.2")].into_iter().enumerate() {
        let payload = serde_json::json!({"target":"Kitchen","command":command}).to_string();
        assert!(broker.send_with(
            "FenceTest/Command/Controller",
            payload,
            &SendOptions::default().user_property("target-generation", generation)
        ));
        let reached = broker.wait_until(Duration::from_secs(2), |_| contexts.lock().unwrap().len() > index).await;
        if !reached {
            assert!(running.stop().await);
            panic!("command never reached its driver");
        }
        let original = contexts.lock().unwrap()[index].generation().map(str::to_owned);
        if original.as_deref() != Some(generation) {
            assert!(running.stop().await);
            panic!("command lost its original generation context");
        }
    }
    assert!(broker.send_with(
        "FenceTest/Command/Controller",
        r#"{"target":"Kitchen","command":"pulse"}"#,
        &SendOptions::default().user_property("target-generation", "epoch.2").user_property("target-generation", "epoch.2")
    ));
    let rejected = broker
        .wait_until(Duration::from_secs(2), |b| {
            b.received_on("FenceTest/Error/Controller")
                .iter()
                .any(|r| String::from_utf8_lossy(&r.payload).contains("target-generation must occur exactly once"))
        })
        .await;
    assert!(running.stop().await);
    assert!(rejected, "duplicate-generation command was not rejected");
    assert_eq!(contexts.lock().unwrap().len(), 2, "duplicate-generation command executed");
}

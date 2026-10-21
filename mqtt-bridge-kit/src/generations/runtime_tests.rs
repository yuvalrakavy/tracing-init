//! Real MQTT and worker paths, using only an ephemeral local fake broker.
use super::*;
use crate::{CommandClass, CommandSpec, DeviceDriver, DriverInfo, LinkContext, LinkError, OpError, Reporter, Scope};
use mqtt_test_broker::{FakeBroker, SendOptions};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::sync::Notify;

fn table(generation: &str) -> TargetGenerations {
    TargetGenerations { instance: generation.into(), targets: [("Kitchen".into(), generation.into())].into_iter().collect() }
}
fn values(level: usize) -> PropertyMap {
    json!({"level":level}).as_object().unwrap().clone()
}

/// Uses the provided contextual-read wrapper around an ordinary legacy driver.
struct UnfencedLegacy(MutableDevice);
impl DeviceDriver for UnfencedLegacy {
    fn info(&self) -> DriverInfo {
        self.0.info()
    }
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
        self.0.connect(ctx).await
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        let level = self.0.level.load(Ordering::SeqCst);
        if self.0.hold_all_reads.swap(false, Ordering::SeqCst) {
            self.0.read_started.store(true, Ordering::SeqCst);
            self.0.release_read.notified().await;
        }
        Ok(vec![TargetState::new("Kitchen", values(level))])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        self.0.calls.lock().unwrap().push(("apply".into(), RequestContext::default()));
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        self.0.calls.lock().unwrap().push(("command".into(), RequestContext::default()));
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

#[tokio::test]
async fn optional_request_context_preserves_unfenced_proving_and_direct_reads() {
    for proving in [false, true] {
        let broker = FakeBroker::start().await;
        let device = MutableDevice::new();
        device.hold_all_reads.store(proving, Ordering::SeqCst);
        let running = crate::Bridge::new("FenceRuntime")
            .instance("Controller")
            .broker(&broker.address())
            .start(UnfencedLegacy(device.clone()))
            .unwrap();
        assert!(broker.wait_for_subscription("FenceRuntime/Command/Controller", Duration::from_secs(3)).await);
        if proving {
            assert!(broker.wait_until(Duration::from_secs(3), |_| device.read_started.load(Ordering::SeqCst)).await);
        } else {
            assert!(
                broker
                    .wait_until(Duration::from_secs(3), |b| b
                        .received_on("FenceRuntime/Status/Controller")
                        .iter()
                        .any(|r| r.payload == "connected"))
                    .await
            );
        }
        assert!(broker.send_with(
            "FenceRuntime/Desired/Controller/Kitchen/level",
            "40",
            &SendOptions::default().user_property("target-generation", "optional.1").retained()
        ));
        // An unsupported command is answered by the MQTT session while proving
        // is paused, establishing that the preceding Desired was admitted.
        assert!(broker.send_with(
            "FenceRuntime/Command/Controller",
            r#"{"command":"unknown"}"#,
            &SendOptions::default().response_topic("Store/legacy-admitted")
        ));
        assert!(broker.wait_until(Duration::from_secs(3), |b| !b.received_on("Store/legacy-admitted").is_empty()).await);
        device.release_read.notify_one();
        // Releasing a proving read does not synchronously make the worker Up.
        // A momentary command posted in that gap is correctly refused.
        assert!(
            broker
                .wait_until(Duration::from_secs(3), |b| b
                    .received_on("FenceRuntime/Status/Controller")
                    .iter()
                    .any(|record| record.payload == "connected"))
                .await
        );
        assert!(broker.send_with(
            "FenceRuntime/Command/Controller",
            r#"{"target":"Kitchen","command":"pulse"}"#,
            &SendOptions::default().response_topic("Store/legacy-finished")
        ));
        assert!(broker.wait_until(Duration::from_secs(3), |b| !b.received_on("Store/legacy-finished").is_empty()).await);
        assert!(running.stop().await);
        assert_eq!(
            *device.calls.lock().unwrap(),
            vec![("command".into(), RequestContext::default())],
            "optional metadata hid a valid legacy read (proving={proving})"
        );
    }
}

#[derive(Clone)]
struct MutableDevice {
    table: Arc<Mutex<TargetGenerations>>,
    reporter: Arc<Mutex<Option<Reporter>>>,
    calls: Arc<Mutex<Vec<(String, RequestContext)>>>,
    level: Arc<AtomicUsize>,
    reject: Arc<AtomicBool>,
    hold_read: Arc<AtomicBool>,
    read_started: Arc<AtomicBool>,
    release_read: Arc<Notify>,
    force_read_context: Arc<Mutex<Option<RequestContext>>>,
    hold_all_reads: Arc<AtomicBool>,
}
impl MutableDevice {
    fn new() -> Self {
        Self {
            table: Arc::new(Mutex::new(table("epoch.1"))),
            reporter: Arc::new(Mutex::new(None)),
            calls: Arc::new(Mutex::new(Vec::new())),
            level: Arc::new(AtomicUsize::new(40)),
            reject: Arc::new(AtomicBool::new(false)),
            hold_read: Arc::new(AtomicBool::new(false)),
            read_started: Arc::new(AtomicBool::new(false)),
            release_read: Arc::new(Notify::new()),
            force_read_context: Arc::new(Mutex::new(None)),
            hold_all_reads: Arc::new(AtomicBool::new(false)),
        }
    }
    fn reporter(&self) -> Reporter {
        self.reporter.lock().unwrap().as_ref().unwrap().clone()
    }
    fn context(&self) -> RequestContext {
        RequestContext::new(&self.table.lock().unwrap().instance).unwrap()
    }
    fn replace(&self, generation: &str) {
        self.reject.store(true, Ordering::SeqCst);
        let next = table(generation);
        let old = self.table.lock().unwrap().clone();
        self.reporter().replace_generations(&old, next.clone()).unwrap();
        *self.table.lock().unwrap() = next;
        self.reject.store(false, Ordering::SeqCst);
    }
    async fn start(&self, broker: &FakeBroker) -> crate::Running {
        let running = crate::Bridge::new("FenceRuntime").instance("Controller").broker(&broker.address()).start(self.clone()).unwrap();
        assert!(broker.wait_for_subscription("FenceRuntime/Desired/Controller/+/+", Duration::from_secs(3)).await);
        assert!(
            broker
                .wait_until(Duration::from_secs(3), |b| b
                    .received_on("FenceRuntime/Status/Controller")
                    .iter()
                    .any(|r| r.payload == "connected"))
                .await
        );
        running
    }
}
impl DeviceDriver for MutableDevice {
    fn info(&self) -> DriverInfo {
        DriverInfo { commands: vec![CommandSpec::new("pulse", CommandClass::Momentary)], ..DriverInfo::default() }
    }
    fn request_policy(&self) -> RequestPolicy {
        RequestPolicy::Fenced(self.table.lock().unwrap().clone())
    }
    fn validate_request(&self, _: Option<&str>, _: &RequestContext, _: RequestKind<'_>) -> Result<(), OpError> {
        if self.reject.load(Ordering::SeqCst) {
            Err(OpError::Rejected("configuration transition active".into()))
        } else {
            Ok(())
        }
    }
    async fn connect(&mut self, context: &mut LinkContext) -> Result<(), LinkError> {
        *self.reporter.lock().unwrap() = Some(context.report.clone());
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Err(OpError::Unsupported("contextual reads only".into()))
    }
    async fn read_state_with_context(&mut self, scope: Scope) -> Result<Vec<ContextualState>, OpError> {
        let context = self.force_read_context.lock().unwrap().clone().unwrap_or_else(|| self.context());
        let level = self.level.load(Ordering::SeqCst);
        if self.hold_all_reads.swap(false, Ordering::SeqCst)
            || (matches!(scope, Scope::Target(_)) && self.hold_read.swap(false, Ordering::SeqCst))
        {
            self.read_started.store(true, Ordering::SeqCst);
            self.release_read.notified().await;
        }
        Ok(vec![ContextualState::new(TargetState::new("Kitchen", values(level)), context)])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Err(OpError::Rejected("lost apply context".into()))
    }
    async fn apply_with_context(&mut self, target: &str, values: &PropertyMap, context: &RequestContext) -> Result<(), OpError> {
        self.calls.lock().unwrap().push(("apply".into(), context.clone()));
        self.reporter().state_with_context(target, values.clone(), false, context);
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        Err(OpError::Rejected("lost command context".into()))
    }
    async fn execute_with_context(&mut self, _: Option<&str>, _: &CommandCall, context: &RequestContext) -> Result<Option<Value>, OpError> {
        self.calls.lock().unwrap().push(("command".into(), context.clone()));
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Err(OpError::Rejected("lost config context".into()))
    }
    async fn configure_with_context(&mut self, _: &str, _: Option<&Value>, context: &RequestContext) -> Result<(), OpError> {
        self.calls.lock().unwrap().push(("configure".into(), context.clone()));
        Ok(())
    }
}

#[tokio::test]
async fn a_late_read_keeps_its_original_reply_context_and_cannot_overwrite_a_replacement() {
    let broker = FakeBroker::start().await;
    let device = MutableDevice::new();
    let running = device.start(&broker).await;
    device.hold_read.store(true, Ordering::SeqCst);
    let options = SendOptions::default().user_property("target-generation", "epoch.1").response_topic("Store/late-read");
    assert!(broker.send_with("FenceRuntime/Command/Controller", r#"{"target":"Kitchen","command":"refresh"}"#, &options));
    assert!(broker.wait_until(Duration::from_secs(3), |_| device.read_started.load(Ordering::SeqCst)).await);
    device.replace("epoch.2");
    device.level.store(70, Ordering::SeqCst);
    device.reporter().state_with_context("Kitchen", values(70), false, &device.context());
    device.release_read.notify_one();
    assert!(broker.send_with(
        "FenceRuntime/Command/Controller",
        r#"{"target":"Kitchen","command":"pulse"}"#,
        &SendOptions::default().user_property("target-generation", "epoch.2").response_topic("Store/read-finished")
    ));
    assert!(
        broker.wait_until(Duration::from_secs(3), |b| !b.received_on("Store/read-finished").is_empty()).await,
        "the worker did not finish folding the late read"
    );
    assert!(broker.wait_until(Duration::from_secs(3), |b| !b.received_on("Store/late-read").is_empty()).await);
    let reply = broker.received_on("Store/late-read").pop().unwrap();
    assert_eq!(reply.user_property("target-generation"), Some("epoch.1"), "late reply lost its original generation");
    // Replacement retires the in-flight read and answers it exactly once.
    assert_eq!(broker.received_on("Store/late-read").len(), 1);
    assert!(
        broker
            .wait_until(Duration::from_secs(3), |b| b
                .received_on("FenceRuntime/State/Controller/Kitchen")
                .last()
                .is_some_and(|r| r.user_property("target-generation") == Some("epoch.2")
                    && serde_json::from_slice::<Value>(&r.payload).ok() == Some(json!({"level":70}))))
            .await,
        "late old observation overwrote the replacement"
    );
    assert!(running.stop().await);
    assert_eq!(*device.calls.lock().unwrap(), vec![("command".into(), RequestContext::new("epoch.2").unwrap())]);
}

#[tokio::test]
async fn driver_validation_runs_before_config_cache_and_already_satisfied_acceptance() {
    let broker = FakeBroker::start().await;
    let device = MutableDevice::new();
    let running = device.start(&broker).await;
    let options = SendOptions::default().user_property("target-generation", "epoch.1");
    assert!(broker.send_with("FenceRuntime/Config/Controller/Kitchen", "{}", &options));
    assert!(broker.wait_until(Duration::from_secs(3), |_| device.calls.lock().unwrap().len() == 1).await);
    device.reject.store(true, Ordering::SeqCst);
    assert!(broker.send_with("FenceRuntime/Config/Controller/Kitchen", "{}", &options));
    assert!(broker.send_with("FenceRuntime/Desired/Controller/Kitchen/level", "40", &options.retained()));
    assert!(
        broker
            .wait_until(Duration::from_secs(3), |b| b
                .received_on("FenceRuntime/Error/Controller")
                .iter()
                .filter(|r| String::from_utf8_lossy(&r.payload).contains("configuration transition active"))
                .count()
                >= 2)
            .await,
        "cache or satisfied conflict bypassed driver admission"
    );
    assert!(running.stop().await);
    assert_eq!(device.calls.lock().unwrap().len(), 1, "guarded work reached hardware");
}

#[tokio::test]
async fn stale_mqtt_mutations_and_assume_never_reach_a_fenced_driver() {
    let broker = FakeBroker::start().await;
    let device = MutableDevice::new();
    let running = device.start(&broker).await;
    device.replace("epoch.2");
    let old = SendOptions::default().user_property("target-generation", "epoch.1");
    for (topic, payload) in [
        ("Desired/Controller/Kitchen/level", "90"),
        ("Config/Controller/Kitchen", "{}"),
        ("Command/Controller", r#"{"target":"Kitchen","command":"pulse"}"#),
        ("Command/Controller", r#"{"target":"Kitchen","command":"assume","level":90}"#),
    ] {
        assert!(broker.send_with(&format!("FenceRuntime/{topic}"), payload, &old));
    }
    assert!(broker.wait_until(Duration::from_secs(3), |b| b.received_on("FenceRuntime/Error/Controller").len() >= 4).await);
    assert!(device.calls.lock().unwrap().is_empty(), "retired MQTT work reached the driver");
    assert!(broker.send_with(
        "FenceRuntime/Desired/Controller/Kitchen/level",
        "60",
        &SendOptions::default().user_property("target-generation", "epoch.2")
    ));
    assert!(broker.wait_until(Duration::from_secs(3), |_| device.calls.lock().unwrap().len() == 1).await);
    assert!(running.stop().await);
    assert_eq!(device.calls.lock().unwrap()[0], ("apply".into(), RequestContext::new("epoch.2").unwrap()));
}

#[tokio::test]
async fn a_stale_direct_read_cannot_satisfy_fresh_held_intent() {
    let broker = FakeBroker::start().await;
    let device = MutableDevice::new();
    let running = device.start(&broker).await;
    device.replace("epoch.2");
    *device.force_read_context.lock().unwrap() = Some(RequestContext::new("epoch.1").unwrap());
    assert!(broker.send_with(
        "FenceRuntime/Desired/Controller/Kitchen/level",
        "40",
        &SendOptions::default().user_property("target-generation", "epoch.2").retained()
    ));
    let applied = broker.wait_until(Duration::from_secs(3), |_| !device.calls.lock().unwrap().is_empty()).await;
    assert!(running.stop().await);
    assert!(applied, "a stale direct read was accepted as already satisfying fresh intent");
    assert_eq!(device.calls.lock().unwrap()[0], ("apply".into(), RequestContext::new("epoch.2").unwrap()));
}

#[tokio::test]
async fn a_stale_proving_read_cannot_satisfy_fresh_held_intent() {
    let broker = FakeBroker::start().await;
    let device = MutableDevice::new();
    *device.force_read_context.lock().unwrap() = Some(RequestContext::new("retired.0").unwrap());
    device.hold_all_reads.store(true, Ordering::SeqCst);
    let running = crate::Bridge::new("FenceRuntime").instance("Controller").broker(&broker.address()).start(device.clone()).unwrap();
    assert!(broker.wait_for_subscription("FenceRuntime/Command/Controller", Duration::from_secs(3)).await);
    assert!(broker.wait_until(Duration::from_secs(3), |_| device.read_started.load(Ordering::SeqCst)).await);
    assert!(broker.send_with(
        "FenceRuntime/Desired/Controller/Kitchen/level",
        "40",
        &SendOptions::default().user_property("target-generation", "epoch.1").retained()
    ));
    // This immediate session answer proves the preceding Desired was admitted
    // while the worker was still awaiting its proving read.
    assert!(broker.send_with(
        "FenceRuntime/Command/Controller",
        r#"{"target":"Kitchen","command":"assume","level":40}"#,
        &SendOptions::default().user_property("target-generation", "epoch.1").response_topic("Store/proving-held")
    ));
    assert!(broker.wait_until(Duration::from_secs(3), |b| !b.received_on("Store/proving-held").is_empty()).await);
    device.release_read.notify_one();
    let applied = broker.wait_until(Duration::from_secs(3), |_| !device.calls.lock().unwrap().is_empty()).await;
    assert!(running.stop().await);
    assert!(applied, "a stale proving read was accepted as already satisfying fresh held intent");
    assert_eq!(device.calls.lock().unwrap()[0], ("apply".into(), RequestContext::new("epoch.1").unwrap()));
}

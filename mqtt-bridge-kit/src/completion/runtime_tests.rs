use super::*;
use crate::*;
use mqtt_test_broker::{FakeBroker, SendOptions};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
    time::Duration,
};
use tokio::sync::Notify;
fn table(revision: u8) -> TargetGenerations {
    let token = format!("epoch.{revision}");
    TargetGenerations { instance: token.clone(), targets: [("Kitchen".into(), token)].into_iter().collect() }
}
type Captured = Arc<Mutex<Option<(Reporter, CommandCompletion)>>>;
#[derive(Clone)]
struct Device {
    table: Arc<Mutex<TargetGenerations>>,
    reporter: Option<Reporter>,
    entered: Arc<AtomicBool>,
    release: Arc<Notify>,
    calls: Arc<AtomicUsize>,
    captured: Captured,
    helper_checked: Arc<AtomicBool>,
}
impl Device {
    fn new() -> Self {
        Self {
            table: Arc::new(Mutex::new(table(1))),
            reporter: None,
            entered: Arc::new(AtomicBool::new(false)),
            release: Arc::new(Notify::new()),
            calls: Arc::new(AtomicUsize::new(0)),
            captured: Default::default(),
            helper_checked: Arc::new(AtomicBool::new(false)),
        }
    }
    async fn start(&self, broker: &FakeBroker) -> Running {
        let running = Bridge::new("Commit").instance("Controller").broker(&broker.address()).start(self.clone()).unwrap();
        assert!(
            broker
                .wait_until(Duration::from_secs(3), |b| b.received_on("Commit/Status/Controller").iter().any(|m| m.payload == "connected"))
                .await
        );
        running
    }
}
impl DeviceDriver for Device {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            commands: vec![CommandSpec::new("rename", CommandClass::WithReply), CommandSpec::new("status", CommandClass::Read)],
            ..DriverInfo::default()
        }
    }
    fn request_policy(&self) -> RequestPolicy {
        RequestPolicy::Fenced(self.table.lock().unwrap().clone())
    }
    fn validate_request(&self, _: Option<&str>, _: &RequestContext, _: RequestKind<'_>) -> Result<(), OpError> {
        assert!(crate::completion::current().is_none(), "validation captured a command capability");
        Ok(())
    }
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
        self.reporter = Some(ctx.report.clone());
        let reporter = ctx.report.clone();
        let checked = self.helper_checked.clone();
        ctx.spawn_transient(async move {
            assert!(reporter.command_completion().is_none(), "helper captured a command capability");
            checked.store(true, Ordering::SeqCst);
        });
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        assert!(crate::completion::current().is_none());
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Err(OpError::Unsupported("none".into()))
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        unreachable!("contextual only")
    }
    async fn execute_with_context(&mut self, _: Option<&str>, call: &CommandCall, _: &RequestContext) -> Result<Option<Value>, OpError> {
        let reporter = self.reporter.as_ref().unwrap().clone();
        if call.command == "status" {
            assert!(reporter.command_completion().is_none(), "read command captured a committing capability");
            return Ok(None);
        }
        let completion = reporter.command_completion().expect("momentary execution has no capability");
        *self.captured.lock().unwrap() = Some((reporter.clone(), completion.clone()));
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.store(true, Ordering::SeqCst);
        self.release.notified().await;
        let old = self.table.lock().unwrap().clone();
        let next = table(2);
        reporter.replace_generations_for_command(&old, next.clone(), &completion).map_err(OpError::Rejected)?;
        *self.table.lock().unwrap() = next;
        Ok(Some(json!({"revision":2})))
    }
}
fn options(topic: &str) -> SendOptions {
    SendOptions::default().user_property("target-generation", "epoch.1").response_topic(topic).correlation_data("original-correlation")
}
#[tokio::test]
async fn a_committing_command_replies_once_with_original_context_while_queued_old_work_is_refused() {
    for targeted in [false, true] {
        let broker = FakeBroker::start().await;
        let device = Device::new();
        let running = device.start(&broker).await;
        assert!(broker.send_with("Commit/Command/Controller", r#"{"command":"status"}"#, &options("test/read")));
        assert!(broker.wait_until(Duration::from_secs(2), |b| !b.received_on("test/read").is_empty()).await);
        let payload = if targeted { r#"{"target":"Kitchen","command":"rename"}"# } else { r#"{"command":"rename"}"# };
        assert!(broker.send_with("Commit/Command/Controller", payload, &options("test/committed")));
        assert!(broker.wait_until(Duration::from_secs(2), |_| device.entered.load(Ordering::SeqCst)).await);
        assert!(broker.send_with("Commit/Command/Controller", payload, &options("test/queued")));
        assert!(broker.send_with("Commit/Command/Controller", r#"{"command":"unknown"}"#, &options("test/barrier")));
        assert!(broker.wait_until(Duration::from_secs(2), |b| !b.received_on("test/barrier").is_empty()).await);
        device.release.notify_one();
        assert!(
            broker
                .wait_until(Duration::from_secs(2), |b| !b.received_on("test/committed").is_empty()
                    && !b.received_on("test/queued").is_empty())
                .await
        );
        assert!(running.stop().await);
        let replies = broker.received_on("test/committed");
        assert_eq!(replies.len(), 1);
        let value: Value = serde_json::from_slice(&replies[0].payload).unwrap();
        assert_eq!(value["value"]["revision"], 2, "committing command lost its successful reply");
        assert!(value.get("error").is_none());
        assert_eq!(replies[0].user_property("target-generation"), Some("epoch.1"));
        assert_eq!(replies[0].correlation_data().unwrap().as_ref(), b"original-correlation");
        let queued = broker.received_on("test/queued");
        assert_eq!(queued.len(), 1);
        assert!(
            serde_json::from_slice::<Value>(&queued[0].payload).unwrap().get("error").is_some(),
            "queued old command survived inventory commit"
        );
        assert_eq!(device.calls.load(Ordering::SeqCst), 1);
        assert!(device.helper_checked.load(Ordering::SeqCst));
    }
}
#[tokio::test]
async fn stop_before_owner_cas_refuses_once_and_a_late_completion_cannot_change_authority() {
    let broker = FakeBroker::start().await;
    let device = Device::new();
    let running = device.start(&broker).await;
    assert!(broker.send_with("Commit/Command/Controller", r#"{"command":"rename"}"#, &options("test/stopped")));
    assert!(broker.wait_until(Duration::from_secs(2), |_| device.entered.load(Ordering::SeqCst)).await);
    let (reporter, completion) = device.captured.lock().unwrap().clone().unwrap();
    assert!(running.stop().await);
    assert!(
        reporter.replace_generations_for_command(&table(1), table(2), &completion).is_err(),
        "late stopped-command capability changed authority"
    );
    let replies = broker.received_on("test/stopped");
    assert_eq!(replies.len(), 1, "stopped committing command did not receive exactly one refusal");
    assert!(serde_json::from_slice::<Value>(&replies[0].payload).unwrap().get("error").is_some());
    assert_eq!(replies[0].user_property("target-generation"), Some("epoch.1"));
    assert_eq!(device.table.lock().unwrap().clone(), table(1));
}

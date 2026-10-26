//! Recovery through the actual pump, subscriptions, managed service and worker.
#[cfg(feature = "mosquitto-acceptance")]
mod mosquitto_tests;
use super::*;
use crate::*;
use mqtt_test_broker::{BrokerEvent, FakeBroker, SendOptions};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[derive(Clone, Default)]
struct Calls {
    starts: Arc<AtomicUsize>,
    connects: Arc<AtomicUsize>,
    applies: Arc<AtomicUsize>,
    commands: Arc<AtomicUsize>,
}
struct Proxy(Calls);
impl DeviceDriver for Proxy {
    fn info(&self) -> DriverInfo {
        DriverInfo { commands: vec![CommandSpec::new("recover", CommandClass::Momentary)], ..Default::default() }
    }
    fn request_policy(&self) -> RequestPolicy {
        RequestPolicy::Fenced(TargetGenerations { instance: "instance.1".into(), targets: BTreeMap::new() })
    }
    async fn connect(&mut self, context: &mut LinkContext) -> Result<(), LinkError> {
        context
            .handle()
            .report()
            .event_with_context(serde_json::json!({"kind":"pending_engine_event"}), &RequestContext::new("instance.1").unwrap());
        self.0.connects.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        self.0.applies.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        self.0.commands.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}
struct Owner {
    calls: Calls,
    handle: oneshot::Sender<RecoveryHandle>,
}
impl ProcessService for Owner {
    type Driver = Proxy;
    async fn run(self, mut context: ServiceContext<Proxy>) -> Result<(), String> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        let recovery = context.recovery().ok_or("missing recovery handle")?;
        context.ready(Proxy(self.calls))?;
        self.handle.send(recovery).map_err(|_| "missing test owner")?;
        context.cancelled().await;
        Ok(())
    }
}
fn image(level: Option<u64>) -> RecoveryImage {
    let mut image = RecoveryImage {
        context: RequestContext::default(),
        generations: TargetGenerations { instance: "instance.1".into(), targets: BTreeMap::new() },
        states: BTreeMap::new(),
        configurations: BTreeMap::new(),
    };
    if let Some(level) = level {
        image.generations.targets.insert("a".into(), "generation.a".into());
        image.states.insert("a".into(), serde_json::json!({"level":level}).as_object().unwrap().clone());
    }
    image
}
fn subscription_count(broker: &FakeBroker, prefix: &str) -> usize {
    broker.events().iter().filter(|event| matches!(event, BrokerEvent::Subscribed(filter) if filter.starts_with(prefix))).count()
}

#[tokio::test]
async fn recovery_cannot_start_without_a_managed_owner_to_finish_its_handshake() {
    let broker = FakeBroker::start().await;
    let result = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start(Proxy(Calls::default()));
    if let Ok(running) = result {
        assert!(running.stop().await);
        panic!("recovery started without a managed owner");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloned_bridge_builders_get_separate_recovery_owners_and_cannot_install_each_others_images() {
    let a = FakeBroker::start().await;
    let b = FakeBroker::start().await;
    let builder = Bridge::new("Recovery").instance("i").recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 });
    let (send_a, receive_a) = oneshot::channel();
    let running_a = builder.clone().broker(&a.address()).start_managed(Owner { calls: Calls::default(), handle: send_a }).await.unwrap();
    let mut handle_a = receive_a.await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), handle_a.readback()).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle_a.complete(first.connection, image(Some(22)), RecoveryReady::Ready, None))
        .await
        .unwrap()
        .unwrap();
    let (send_b, receive_b) = oneshot::channel();
    let running_b = builder.broker(&b.address()).start_managed(Owner { calls: Calls::default(), handle: send_b }).await.unwrap();
    let mut handle_b = receive_b.await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(2), handle_b.readback()).await.unwrap().unwrap();
    assert_ne!(first.connection, second.connection, "cloned recovery builder reused another bridge's replay");
    let foreign =
        tokio::time::timeout(Duration::from_secs(2), handle_a.complete(second.connection, image(Some(99)), RecoveryReady::Ready, None))
            .await
            .unwrap();
    if foreign.is_ok() {
        assert!(running_a.stop().await);
        assert!(running_b.stop().await);
        panic!("cloned recovery owners accepted each other's recovery connection");
    }
    tokio::time::timeout(Duration::from_secs(2), handle_b.complete(second.connection, image(Some(33)), RecoveryReady::Ready, None))
        .await
        .unwrap()
        .unwrap();
    assert!(
        a.wait_until(Duration::from_secs(2), |broker| broker
            .received_on("Recovery/State/i/a")
            .iter()
            .any(|publish| serde_json::from_slice::<Value>(&publish.payload).unwrap()["level"] == 22))
            .await
    );
    assert!(
        b.wait_until(Duration::from_secs(2), |broker| broker
            .received_on("Recovery/State/i/a")
            .iter()
            .any(|publish| serde_json::from_slice::<Value>(&publish.payload).unwrap()["level"] == 33))
            .await
    );
    assert!(!a
        .received_on("Recovery/State/i/a")
        .iter()
        .any(|publish| serde_json::from_slice::<Value>(&publish.payload).unwrap()["level"] == 99));
    assert!(running_a.stop().await);
    assert!(running_b.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_before_recovery_install_never_flushes_queued_engine_output() {
    let broker = FakeBroker::start().await;
    broker.hold_subacks_matching("Recovery/State/i/_sync/");
    let calls = Calls::default();
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start_managed(Owner { calls: calls.clone(), handle: send })
        .await
        .unwrap();
    let _handle = receive.await.unwrap();
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |broker| calls.connects.load(Ordering::SeqCst) == 1
                && subscription_count(broker, "Recovery/State/i/_sync/") == 1)
            .await
    );
    assert!(running.stop().await);
    assert!(broker.received_on("Recovery/Event/i").is_empty(), "shutdown flushed engine output before recovery installation");
    assert!(broker.received_on("Recovery/Status/i").is_empty());
    assert!(broker.received_on("Recovery/Active/i").iter().all(|publish| publish.payload != "true"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_engine_bootstrap_expires_the_connection_without_restarting_the_service() {
    let broker = FakeBroker::start().await;
    let calls = Calls::default();
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start_managed(Owner { calls: calls.clone(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    assert!(
        broker.wait_until(Duration::from_secs(13), |broker| subscription_count(broker, "Recovery/State/i/_sync/") >= 2).await,
        "an unanswered engine decision left its connection open indefinitely"
    );
    let second = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    assert_ne!(first.connection, second.connection);
    assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
    assert!(broker.received_on("Recovery/Active/i").is_empty());
    assert!(broker.received_on("Recovery/State/i/a").is_empty());
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_only_refusals_answer_correlated_callers_and_do_not_run_builtin_refresh() {
    let broker = FakeBroker::start().await;
    let calls = Calls::default();
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start_managed(Owner { calls: calls.clone(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    let selected = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    handle.complete(selected.connection, image(None), RecoveryReady::RecoveryOnly, None).await.unwrap();
    assert!(broker.wait_for_subscription("Recovery/Command/i", Duration::from_secs(2)).await);
    for (index, body) in [r#"{"target":"a","command":"recover"}"#, r#"{"command":"refresh"}"#].into_iter().enumerate() {
        let id = format!("{index:064x}");
        let topic = format!("Store/recovery-refused-{index}");
        let correlation = Bytes::from(format!("blocked-{index}"));
        let options =
            SendOptions::default().expiry(10).response_topic(&topic).correlation_data(correlation.clone()).user_property("request-id", &id);
        assert!(broker.send_with("Recovery/Command/i", body, &options));
        assert!(
            broker.wait_until(Duration::from_secs(2), |broker| !broker.received_on(&topic).is_empty()).await,
            "recovery refusal did not answer its correlated caller"
        );
        let replies = broker.received_on(&topic);
        assert_eq!(replies.len(), 1);
        assert_eq!(replies[0].correlation_data(), Some(&correlation));
        let reply: Value = serde_json::from_slice(&replies[0].payload).unwrap();
        assert_eq!(reply["reason"], "refused", "a builtin refresh bypassed recovery-only admission");
        assert!(reply["error"].is_string());
        assert!(broker
            .received_on("Recovery/Error/i")
            .iter()
            .any(|publish| serde_json::from_slice::<Value>(&publish.payload).unwrap()["request-id"] == id));
    }
    assert_eq!(calls.commands.load(Ordering::SeqCst), 0);
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_connection_needs_its_own_barrier_and_install_before_state_or_ordinary_requests() {
    let broker = FakeBroker::start().await;
    let snapshot = crate::wire::encode_state(&serde_json::json!({"snapshot":"selected"}).as_object().unwrap().clone(), false);
    broker.retain("Recovery/State/i/_recovery", snapshot.clone(), &SendOptions::default());
    broker.hold_subacks_matching("Recovery/State/i/_sync/");
    let calls = Calls::default();
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start_managed(Owner { calls: calls.clone(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    assert!(broker.wait_until(Duration::from_secs(2), |broker| subscription_count(broker, "Recovery/State/i/_sync/") == 1).await);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), handle.readback()).await.is_err(),
        "snapshot subscription alone completed recovery replay"
    );
    assert_eq!(subscription_count(&broker, "Recovery/Desired/"), 0);
    assert!(broker.received_on("Recovery/Active/i").is_empty());
    broker.release_subacks();
    let first = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    assert_eq!(first.document.unwrap(), Some(Bytes::from(snapshot)));
    assert_eq!(subscription_count(&broker, "Recovery/Desired/"), 0, "ordinary lanes were subscribed before engine installation");
    assert!(broker.received_on("Recovery/Active/i").is_empty(), "replay completion alone announced recovery readiness");
    assert!(broker.received_on("Recovery/State/i/a").is_empty(), "state overwrote broker data before engine installation");
    tokio::time::timeout(Duration::from_secs(2), handle.complete(first.connection, image(Some(22)), RecoveryReady::Ready, None))
        .await
        .unwrap()
        .unwrap();
    assert!(broker.wait_until(Duration::from_secs(2), |broker| !broker.received_on("Recovery/State/i/a").is_empty()).await);
    let desired =
        SendOptions::default().expiry(10).user_property("target-generation", "generation.a").user_property("request-id", &"ab".repeat(32));
    assert!(broker.send_with("Recovery/Desired/i/a/level", "23", &desired));
    assert!(broker.wait_until(Duration::from_secs(2), |_| calls.applies.load(Ordering::SeqCst) == 1).await);

    broker.hold_subacks_matching("Recovery/State/i/_sync/");
    broker.drop_connection();
    assert!(broker.wait_until(Duration::from_secs(3), |broker| subscription_count(broker, "Recovery/State/i/_sync/") == 2).await);
    assert!(
        handle.complete(first.connection, image(Some(99)), RecoveryReady::Ready, None).await.is_err(),
        "old connection completion reopened recovery"
    );
    assert_eq!(calls.starts.load(Ordering::SeqCst), 1, "MQTT reconnect restarted the installation service");
    assert!(tokio::time::timeout(Duration::from_millis(30), handle.readback()).await.is_err());
    broker.release_subacks();
    let second = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(2), handle.complete(second.connection, image(None), RecoveryReady::RecoveryOnly, None))
        .await
        .unwrap()
        .unwrap();
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |broker| broker
                .received_on("Recovery/Active/i")
                .iter()
                .filter(|publish| publish.payload == "true")
                .count()
                == 2)
            .await
    );
    let errors_before = broker.received_on("Recovery/Error/i").len();
    assert!(broker.send_with("Recovery/Desired/i/a/level", "44", &desired));
    assert!(broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Recovery/Error/i").len() > errors_before).await);
    assert_eq!(calls.applies.load(Ordering::SeqCst), 1, "recovery-only admission accepted ordinary lighting work");
    assert!(
        broker.received_on("Recovery/Error/i").iter().any(|publish| {
            let error: Value = serde_json::from_slice(&publish.payload).unwrap();
            error["request-id"] == "ab".repeat(32) && error["target"] == "a" && error["property"] == "level"
        }),
        "recovery refusal lost operation identity or target/property metadata"
    );
    assert!(broker.send_with(
        "Recovery/Command/i",
        r#"{"command":"recover"}"#,
        &SendOptions::default().expiry(10).user_property("target-generation", "instance.1")
    ));
    assert!(
        broker.wait_until(Duration::from_secs(2), |_| calls.commands.load(Ordering::SeqCst) == 1).await,
        "recovery command was unreachable while recovery-only"
    );
    tokio::time::timeout(Duration::from_secs(2), handle.complete(second.connection, image(Some(33)), RecoveryReady::Ready, None))
        .await
        .unwrap()
        .unwrap();
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |broker| broker
                .received_on("Recovery/State/i/a")
                .iter()
                .any(|publish| serde_json::from_slice::<Value>(&publish.payload).unwrap()["level"] == 33))
            .await
    );
    assert!(broker.pkid_reuses().is_empty());
    assert!(running.stop().await);
}

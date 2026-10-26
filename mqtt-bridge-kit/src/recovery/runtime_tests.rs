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
        snapshot: None,
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

fn snapshot(revision: u64) -> RecoverySnapshot {
    RecoverySnapshot::new(revision, Bytes::from(format!("{{\"snapshot\":\"revision-{revision}\"}}")), RequestContext::default()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_negative_snapshot_puback_is_a_fault_with_its_exact_identity_and_does_not_restart_the_service() {
    let broker = FakeBroker::start().await;
    broker.deny_publications("Recovery/State/i/_recovery");
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
    let readback = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    let rejected = snapshot(41);
    let mut selected = image(Some(22));
    selected.snapshot = Some(rejected.clone());
    handle.complete(readback.connection, selected, RecoveryReady::Ready, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(receipt) = handle.receipt() {
                assert_eq!(receipt.snapshot_id, rejected.id());
                assert_eq!(receipt.revision, 41);
                assert_eq!(receipt.connection, readback.connection);
                assert!(receipt.packet.is_some_and(|packet| packet != 0));
                assert!(receipt.result.unwrap_err().contains("NotAuthorized"), "negative PUBACK was mistaken for broker acceptance");
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("a negative snapshot PUBACK produced no receipt");
    assert!(broker.wait_for_subscription("Recovery/Desired/i/+/+", Duration::from_secs(2)).await);
    assert!(broker.send_with(
        "Recovery/Desired/i/a/level",
        "23",
        &SendOptions { user_properties: vec![("target-generation".into(), "generation.a".into())], ..Default::default() }
    ));
    assert!(broker.wait_until(Duration::from_secs(2), |_| calls.applies.load(Ordering::SeqCst) == 1).await);
    assert_eq!(broker.received_on("Recovery/State/i/_recovery").len(), 1);
    assert_eq!(broker.accepted(), 1);
    assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_snapshot_wakes_after_the_publication_pacing_interval_without_other_traffic() {
    let broker = FakeBroker::start().await;
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start_managed(Owner { calls: Calls::default(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    let readback = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    let mut selected = image(None);
    selected.snapshot = Some(snapshot(1));
    let began = std::time::Instant::now();
    handle.complete(readback.connection, selected, RecoveryReady::Ready, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while handle.receipt().is_none() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert!(began.elapsed() < Duration::from_millis(90), "fixture missed the pacing window");
    handle.publish(readback.connection, snapshot(2)).unwrap();
    assert!(
        broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Recovery/State/i/_recovery").len() == 2).await,
        "pending snapshot was stranded without a publication timer"
    );
    assert!(began.elapsed() >= Duration::from_millis(95), "snapshots were published faster than once per 100ms");
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_discards_old_snapshot_work_and_only_publishes_the_newly_selected_image() {
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
    let mut selected = image(None);
    selected.snapshot = Some(snapshot(1));
    broker.hold_acks();
    handle.complete(first.connection, selected, RecoveryReady::Ready, None).await.unwrap();
    assert!(broker.wait_until(Duration::from_secs(2), |broker| !broker.received_on("Recovery/State/i/_recovery").is_empty()).await);
    handle.publish(first.connection, snapshot(2)).unwrap();
    broker.hold_subacks_matching("Recovery/State/i/_sync/");
    broker.drop_connection();
    assert!(broker.wait_until(Duration::from_secs(3), |broker| subscription_count(broker, "Recovery/State/i/_sync/") >= 2).await);
    assert!(handle.receipt().is_none());
    assert!(handle.publish(first.connection, snapshot(3)).is_err());
    assert_eq!(broker.received_on("Recovery/State/i/_recovery").len(), 1, "old snapshot work escaped before the new barrier");
    broker.release_acks();
    broker.release_subacks();
    let second = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    assert_ne!(first.connection, second.connection);
    let mut replacement = image(None);
    let latest = snapshot(4);
    replacement.snapshot = Some(latest.clone());
    handle.complete(second.connection, replacement, RecoveryReady::Ready, None).await.unwrap();
    assert!(broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Recovery/State/i/_recovery").len() == 2).await);
    assert_eq!(
        broker.received_on("Recovery/State/i/_recovery")[1].payload,
        *latest.document(),
        "new connection published a stale pending snapshot"
    );
    assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_snapshot_above_the_brokers_packet_ceiling_reports_a_fault_without_reconnecting() {
    assert_snapshot_ceiling(1024, 2000).await;
    assert_snapshot_ceiling(u32::MAX, crate::session::MAX_PACKET_SIZE as usize).await;
}

async fn assert_snapshot_ceiling(maximum: u32, payload_bytes: usize) {
    let broker = FakeBroker::start().await;
    broker.advertise_maximum_packet_size(maximum);
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: crate::session::MAX_PACKET_SIZE as usize })
        .start_managed(Owner { calls: Calls::default(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    let readback = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    let oversized = RecoverySnapshot::new(
        1,
        Bytes::from(format!("{{\"snapshot\":\"{}\"}}", "x".repeat(payload_bytes - 15))),
        RequestContext::default(),
    )
    .unwrap();
    assert_eq!(oversized.document().len(), payload_bytes);
    let mut selected = image(None);
    selected.snapshot = Some(oversized.clone());
    handle.complete(readback.connection, selected, RecoveryReady::Ready, None).await.unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(receipt) = handle.receipt() {
                assert_eq!(receipt.snapshot_id, oversized.id());
                assert!(receipt.packet.is_none(), "oversized snapshot was sent despite the advertised ceiling");
                assert!(receipt.result.unwrap_err().contains("maximum"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("oversized snapshot produced no explicit publication fault");
    assert!(broker.received_on("Recovery/State/i/_recovery").is_empty());
    handle.publish(readback.connection, snapshot(2)).unwrap();
    assert!(broker.wait_until(Duration::from_secs(2), |broker| !broker.received_on("Recovery/State/i/_recovery").is_empty()).await);
    assert_eq!(broker.accepted(), 1, "a snapshot size fault caused a reconnect loop");
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshots_publish_through_the_runtime_and_receipts_name_the_immutable_inflight_revision() {
    let broker = FakeBroker::start().await;
    let (send, receive) = oneshot::channel();
    let running = Bridge::new("Recovery")
        .instance("i")
        .broker(&broker.address())
        .recovery(RecoveryOptions { target: "_recovery".into(), max_bytes: 4096 })
        .start_managed(Owner { calls: Calls::default(), handle: send })
        .await
        .unwrap();
    let mut handle = receive.await.unwrap();
    let readback = tokio::time::timeout(Duration::from_secs(2), handle.readback()).await.unwrap().unwrap();
    let first = snapshot(7);
    let mut selected = image(Some(22));
    selected.snapshot = Some(first.clone());
    broker.hold_acks();
    handle.complete(readback.connection, selected, RecoveryReady::Ready, None).await.unwrap();
    assert!(
        broker.wait_until(Duration::from_secs(2), |broker| !broker.received_on("Recovery/State/i/_recovery").is_empty()).await,
        "the installed snapshot never reached the existing runtime publisher"
    );
    assert!(handle.receipt().is_none(), "enqueuing a snapshot was mistaken for broker acceptance");
    for revision in 8..=20 {
        handle.publish(readback.connection, snapshot(revision)).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(130)).await;
    assert_eq!(broker.received_on("Recovery/State/i/_recovery").len(), 1, "a second snapshot escaped while its predecessor was in flight");
    broker.delay_acks(Duration::from_secs(1));
    broker.release_acks();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(receipt) = handle.receipt() {
                assert_eq!(receipt.snapshot_id, first.id());
                assert_eq!(receipt.revision, 7, "the old packet was credited to a newer pending revision");
                assert_eq!(receipt.connection, readback.connection);
                assert!(receipt.packet.is_some_and(|packet| packet != 0));
                assert!(receipt.result.is_ok());
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the broker's exact PUBACK produced no receipt");
    assert!(broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Recovery/State/i/_recovery").len() == 2).await);
    let sent = broker.received_on("Recovery/State/i/_recovery");
    assert_eq!(sent[1].payload, snapshot(20).document().clone(), "pending snapshots did not coalesce to the latest revision");
    assert!(sent.iter().all(|publish| publish.retain && publish.qos == mqtt_test_broker::QoS::AtLeastOnce));
    assert!(running.stop().await);
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

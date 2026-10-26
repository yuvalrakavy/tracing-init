use crate::*;
use mqtt_test_broker::{FakeBroker, SendOptions};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Clone, Default)]
struct Activity {
    starts: Arc<AtomicUsize>,
    ticks: Arc<AtomicUsize>,
}
struct Owner(Activity);
struct Proxy;
impl ProcessService for Owner {
    type Driver = Proxy;
    async fn run(self, mut context: ServiceContext<Proxy>) -> Result<(), String> {
        self.0.starts.fetch_add(1, Ordering::SeqCst);
        context.ready(Proxy)?;
        let mut ticker = tokio::time::interval(Duration::from_millis(5));
        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                _ = ticker.tick() => { self.0.ticks.fetch_add(1, Ordering::SeqCst); }
            }
        }
    }
}
impl DeviceDriver for Proxy {
    fn info(&self) -> DriverInfo {
        DriverInfo { commands: vec![CommandSpec::new("echo", CommandClass::Momentary)], ..Default::default() }
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
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> {
        Ok(Some(Value::String("x".repeat(128))))
    }
}

async fn start(broker: &FakeBroker, activity: &Activity, limits: BufferLimits) -> Running {
    let running = Bridge::new("Buffers")
        .instance("i")
        .broker(&broker.address())
        .buffers(limits)
        .start_managed(Owner(activity.clone()))
        .await
        .unwrap();
    assert!(broker.wait_for_subscription("Buffers/Command/i", Duration::from_secs(2)).await);
    assert!(
        broker
            .wait_until(Duration::from_secs(2), |broker| broker
                .events()
                .iter()
                .any(|event| matches!(event, mqtt_test_broker::BrokerEvent::Unsubscribed(filter) if filter == "Buffers/State/i/#")))
            .await,
        "fixture did not finish bootstrap before holding publication acknowledgments"
    );
    running
}

#[tokio::test]
async fn buffer_limits_refuse_zero_before_starting_the_managed_service() {
    let broker = FakeBroker::start().await;
    for limits in [BufferLimits { incoming_bytes: 0, reply_bytes: 1 }, BufferLimits { incoming_bytes: 1, reply_bytes: 0 }] {
        let activity = Activity::default();
        let result =
            Bridge::new("Buffers").instance("i").broker(&broker.address()).buffers(limits).start_managed(Owner(activity.clone())).await;
        match result {
            Err(error) => {
                assert!(error.contains("buffer limits"), "fixture refused something other than the invalid buffer limits: {error}")
            }
            Ok(running) => {
                assert!(running.stop().await);
                panic!("invalid buffer limits started a bridge");
            }
        }
        assert_eq!(activity.starts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incoming_budget_exhaustion_retries_mqtt_while_the_engine_keeps_ticking() {
    let broker = FakeBroker::start().await;
    let activity = Activity::default();
    let running = start(&broker, &activity, BufferLimits { incoming_bytes: 1024, reply_bytes: 4096 }).await;
    let before = activity.ticks.load(Ordering::SeqCst);
    assert!(broker.send("Buffers/Command/i", format!("{{\"command\":\"echo\",\"data\":\"{}\"}}", "x".repeat(2048))));
    assert!(
        broker.wait_until(Duration::from_secs(3), |broker| broker.accepted() >= 2).await,
        "incoming budget exhaustion did not retry the MQTT connection"
    );
    assert_eq!(activity.starts.load(Ordering::SeqCst), 1, "MQTT overload restarted the managed engine");
    assert!(activity.ticks.load(Ordering::SeqCst) > before + 10, "MQTT overload stopped the engine timeline");
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replies_keep_their_budget_through_stalled_pubacks_and_retry_only_mqtt() {
    let broker = FakeBroker::start().await;
    let activity = Activity::default();
    let running = start(&broker, &activity, BufferLimits { incoming_bytes: 65536, reply_bytes: 1024 }).await;
    broker.hold_acks();
    let before = activity.ticks.load(Ordering::SeqCst);
    let options = SendOptions { response_topic: Some("Client/replies".into()), ..Default::default() };
    for count in 1..=2 {
        assert!(broker.send_with("Buffers/Command/i", r#"{"command":"echo"}"#, &options));
        assert!(
            broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Client/replies").len() == count).await,
            "fixture did not put reply {count} into MQTT flight; connections={}, events={:?}, received={:?}",
            broker.accepted(),
            broker.events(),
            broker.received()
        );
    }
    assert!(broker.send_with("Buffers/Command/i", r#"{"command":"echo"}"#, &options));
    assert!(
        broker.wait_until(Duration::from_secs(3), |broker| broker.accepted() >= 2).await,
        "reply budget exhaustion did not interrupt stalled MQTT publication"
    );
    broker.release_acks();
    assert_eq!(activity.starts.load(Ordering::SeqCst), 1);
    assert!(activity.ticks.load(Ordering::SeqCst) > before + 10, "reply overload stopped the engine timeline");
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn acknowledged_replies_release_capacity_for_later_commands_without_reconnecting() {
    let broker = FakeBroker::start().await;
    let activity = Activity::default();
    let running = start(&broker, &activity, BufferLimits { incoming_bytes: 65536, reply_bytes: 1024 }).await;
    let options = SendOptions { response_topic: Some("Client/replies".into()), ..Default::default() };
    for count in 1..=30 {
        assert!(broker.send_with("Buffers/Command/i", r#"{"command":"echo"}"#, &options));
        assert!(
            broker.wait_until(Duration::from_secs(2), |broker| broker.received_on("Client/replies").len() == count).await,
            "acknowledged replies kept their byte charge and prevented later commands"
        );
    }
    assert_eq!(broker.accepted(), 1, "healthy acknowledged traffic exhausted reply capacity");
    assert_eq!(activity.starts.load(Ordering::SeqCst), 1);
    assert!(running.stop().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unread_pump_publications_are_aggregate_bounded_and_queue_drop_releases_their_charges() {
    use rumqttc::v5::{AsyncClient, MqttOptions};
    let broker = FakeBroker::start().await;
    let buffers = super::Buffers::new(BufferLimits { incoming_bytes: 1024, reply_bytes: 1024 });
    let connection = buffers.begin();
    let mut options = MqttOptions::new("buffered-pump", "127.0.0.1", broker.port());
    options.set_keep_alive(Duration::from_secs(5));
    let (client, events) = AsyncClient::new(options, 10);
    let outage = Arc::new(crate::pump::BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(30)));
    let (_pump, incoming) =
        crate::pump::Pump::start_with_buffers(events, outage.clone(), Duration::from_secs(10), || {}, false, Some(connection.clone()));
    client.subscribe("Buffers/input", rumqttc::v5::mqttbytes::QoS::AtLeastOnce).await.unwrap();
    assert!(broker.wait_for_subscription("Buffers/input", Duration::from_secs(2)).await);
    for _ in 0..20 {
        let _ = broker.send("Buffers/input", "{}");
    }
    assert!(
        broker.wait_until(Duration::from_secs(2), |_| incoming.ended()).await,
        "many small unread publishes escaped the aggregate incoming budget"
    );
    let used = buffers.incoming.used.load(Ordering::SeqCst);
    assert!(used > 0 && used <= 1024, "incoming queued bytes exceeded their bound");
    assert!(matches!(connection.reason(), Some(super::Overflow::Incoming)));
    assert!(!outage.down(), "local buffer exhaustion was mislabeled as an unreachable broker");
    drop(incoming);
    assert_eq!(buffers.incoming.used.load(Ordering::SeqCst), 0, "dropping the connection kept unread input charged");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cloned_builders_do_not_share_buffer_exhaustion_or_restart_each_others_engines() {
    let a = FakeBroker::start().await;
    let b = FakeBroker::start().await;
    let first = Activity::default();
    let other = Activity::default();
    let builder = Bridge::new("Buffers").instance("i").buffers(BufferLimits { incoming_bytes: 1024, reply_bytes: 1024 });
    let a_running = builder.clone().broker(&a.address()).start_managed(Owner(first.clone())).await.unwrap();
    let b_running = builder.broker(&b.address()).start_managed(Owner(other.clone())).await.unwrap();
    assert!(a.wait_for_subscription("Buffers/Command/i", Duration::from_secs(2)).await);
    assert!(b.wait_for_subscription("Buffers/Command/i", Duration::from_secs(2)).await);
    assert!(a.send("Buffers/Command/i", format!("{{\"command\":\"echo\",\"data\":\"{}\"}}", "x".repeat(2048))));
    assert!(a.wait_until(Duration::from_secs(3), |broker| broker.accepted() >= 2).await);
    let options = SendOptions { response_topic: Some("Client/replies".into()), ..Default::default() };
    assert!(b.send_with("Buffers/Command/i", r#"{"command":"echo"}"#, &options));
    assert!(
        b.wait_until(Duration::from_secs(2), |broker| broker.received_on("Client/replies").len() == 1).await,
        "another bridge's buffering state prevented a healthy reply"
    );
    assert_eq!(b.accepted(), 1, "one bridge's overflow ended another bridge's connection");
    assert_eq!(first.starts.load(Ordering::SeqCst), 1);
    assert_eq!(other.starts.load(Ordering::SeqCst), 1);
    assert!(a_running.stop().await);
    assert!(b_running.stop().await);
}

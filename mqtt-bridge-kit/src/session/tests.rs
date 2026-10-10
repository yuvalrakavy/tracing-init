//! The session on its own, against FakeBroker: what it does with a publish rumqttc refuses.

use std::sync::Arc;
use std::time::Duration;

use mqtt_test_broker::FakeBroker;

use super::*;
use crate::mailbox::Mailbox;
use crate::pump::BrokerOutage;
use crate::reports::Reports;

/// A fixed bridge's session parts on `port`: the session part, its one device, the rotation and
/// the mode.
struct Rig {
    shared: Arc<Shared>,
    device: Arc<Device>,
    outputs: Outputs,
    mode: Mode,
    events: tokio::sync::mpsc::UnboundedReceiver<crate::devices::Event>,
    _events: tokio::sync::mpsc::UnboundedSender<crate::devices::Event>,
}

fn rig_on(port: u16) -> Rig {
    let topics = Topics::new("T", "session").expect("topics");
    let ready = Arc::new(tokio::sync::Notify::new());
    let shared = Arc::new(Shared {
        topics: topics.clone(),
        broker_host: "127.0.0.1".into(),
        broker_port: port,
        timing: MqttTiming::default(),
        version: "test".into(),
        outage: Arc::new(BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5))),
        enqueued: std::sync::atomic::AtomicU64::new(0),
        ready: ready.clone(),
        devices: lock_order::sync::Mutex::new("session-devices", Vec::new()),
    });
    let device = Arc::new(Device {
        topics,
        commands: Vec::new().into(),
        mailbox: Mailbox::new(),
        reports: Reports::with_ready(ready),
        tag: None,
        span: tracing::Span::none(),
    });
    let mut outputs = Outputs::new();
    outputs.add(&shared, 0, device.clone());
    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
    Rig { shared, device, outputs, mode: Mode::Fixed(Fixed::default()), events, _events: events_tx }
}

/// A publish rumqttc refuses (a topic holding a wildcard) is one message dropped: the ones after it
/// still go out (review B2: the first refusal ended the CONNACK duties, and the session never
/// published a report again).
#[tokio::test(flavor = "multi_thread")]
async fn a_refused_publish_is_one_message_dropped() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    let model = Model { status: None, states: vec![("a#b".to_owned(), b"{}".to_vec()), ("t1".to_owned(), br#"{"x":1}"#.to_vec())] };
    let republished = tokio::time::timeout(Duration::from_secs(5), republish(&shared, &client, &device, model)).await;
    assert!(republished.is_ok(), "the republish did not end");
    let t1 = shared.topics.state("t1");
    assert!(
        broker.wait_until(Duration::from_secs(5), |b| !b.received_on(&t1).is_empty()).await,
        "the state after a refused one was never published"
    );
}

/// The publication rotation (multi-device spec §4.7): at most a quantum (32) of one device's
/// messages a turn; the next turn the next device with output, the cursor kept; and a device's batch
/// sealed when taken — its reply goes out with that batch, ahead of State reported after it.
#[tokio::test(flavor = "multi_thread")]
async fn the_rotation_turns_quanta_round_robin_over_sealed_batches() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, first) = (rig.shared.clone(), rig.device.clone());
    let second = Arc::new(Device {
        topics: Topics::new("T", "second").expect("topics"),
        commands: Vec::new().into(),
        mailbox: Mailbox::new(),
        reports: Reports::with_ready(shared.ready.clone()),
        tag: None,
        span: tracing::Span::none(),
    });
    rig.outputs.add(&shared, 1, second.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let connected = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(connected, Some(PumpEvent::Connected)));
    assert!(rig.outputs.announce(0) && rig.outputs.announce(1));
    let state = |n: usize| serde_json::json!({"n": n}).as_object().unwrap().clone();
    for t in 0..100 {
        first.reports.state(&format!("m{t:03}"), state(t), false);
    }
    first.reports.event(Outbound { topic: Some("Store/replies/sealed".into()), payload: b"{}".to_vec(), correlation: None });
    second.reports.state("s1", state(1), false);
    let handed = |shared: &Shared| shared.enqueued.load(std::sync::atomic::Ordering::SeqCst);
    let mut served = Vec::new();
    served.push(rig.outputs.turn(&shared, &client).await);
    assert_eq!(handed(&shared), 32, "a turn handed over more than a quantum of one device's messages");
    served.push(rig.outputs.turn(&shared, &client).await);
    // State reported after the first device's batch was taken: the next batch's.
    for t in 0..50 {
        first.reports.state(&format!("n{t:03}"), state(t), false);
    }
    for _ in 0..5 {
        served.push(rig.outputs.turn(&shared, &client).await);
    }
    assert_eq!(&served[..2], &[Some(0), Some(1)], "the second turn was not the next device's: the cursor stayed");
    let total = 100 + 1 + 1 + 50;
    assert!(broker.wait_until(Duration::from_secs(5), |b| b.received().len() >= total).await, "not everything was published");
    let received = broker.received();
    let reply = received.iter().position(|r| r.topic == "Store/replies/sealed").expect("the reply");
    let later = received.iter().position(|r| r.topic == first.topics.state("n000")).expect("the later State");
    assert!(reply < later, "the reply went out behind State reported after its batch was taken: the batch was not sealed");
}

/// Only a retained `State` is read back: a live one — the bridge's own publish coming back on its
/// read-back subscription, or another client's — is no record of what the previous run knew, and
/// no "before" value for the conflict rule.
#[test]
fn a_live_state_is_not_read_back() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    let topic = device.topics.state("t1");
    let live = Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"temp":1}"#.to_vec(), None);
    route(&device, &topic, &live, Instant::now());
    assert!(device.reports.read_back_before().1.is_empty(), "a live State was read back");
    let mut retained = Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"temp":2}"#.to_vec(), None);
    retained.retain = true;
    route(&device, &topic, &retained, Instant::now());
    assert_eq!(device.reports.read_back_before().1["t1"]["temp"], serde_json::json!(2));
}

/// The session's panic seam is off without the feature (the re-gate 3 S5).
#[cfg(not(feature = "test-seams"))]
#[test]
fn the_panic_seam_is_off_without_its_feature() {
    std::env::set_var(TEST_PANIC_TOPIC, "T/seam");
    let ran = std::panic::catch_unwind(|| panic_for_test("T/seam"));
    std::env::remove_var(TEST_PANIC_TOPIC);
    assert!(ran.is_ok(), "the panic seam is on in a build without the test-seams feature");
}

/// A retraction settles an unproven assumption of its key: the Store saw the assumed State and
/// confirmed it, so the conflict rule takes the assumed value, not the one before it (re-gate 3 S6).
#[test]
fn a_retraction_settles_the_keys_unproven_assumption() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    device.reports.assume_unproven("t1", serde_json::json!({"level": 40}).as_object().unwrap().clone());
    assert_eq!(device.reports.values_of("t1").and_then(|v| v.get("level").cloned()), None);
    let topic = device.topics.desired("t1", "level");
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&device, &topic, &retraction, Instant::now());
    assert_eq!(
        device.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        Some(serde_json::json!(40)),
        "a retraction left the key's assumption unproven"
    );
}

/// A retraction of a key whose write the mailbox holds unproven settles nothing: the probe proves
/// it, or the link's loss holds it again (re-gate 4 minor 7).
#[test]
fn a_retraction_of_an_unproven_write_leaves_its_assumption_unproven() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    device.mailbox.connected();
    let level = serde_json::json!({"level": 40}).as_object().unwrap().clone();
    let topic = device.topics.desired("t1", "level");
    let mut request = Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), None);
    request.retain = false;
    route(&device, &topic, &request, Instant::now());
    assert!(device.mailbox.try_take().is_some());
    device.mailbox.applied(true);
    device.reports.assume_unproven("t1", level);
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&device, &topic, &retraction, Instant::now());
    assert_eq!(
        device.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        None,
        "a retraction proved a write the device never proved taken"
    );
}

/// The same after the link was lost: the write is held again, unproven, and a confirmation then
/// settles nothing (Codex final gate 5).
#[test]
fn a_retraction_after_the_re_hold_leaves_its_assumption_unproven() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    device.mailbox.connected();
    let level = serde_json::json!({"level": 40}).as_object().unwrap().clone();
    let topic = device.topics.desired("t1", "level");
    let request = Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), None);
    route(&device, &topic, &request, Instant::now());
    assert!(device.mailbox.try_take().is_some());
    device.mailbox.applied(true);
    device.reports.assume_unproven("t1", level);
    let _ = device.mailbox.disconnected();
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&device, &topic, &retraction, Instant::now());
    assert_eq!(
        device.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        None,
        "a confirmation after the re-hold proved a write the device never took"
    );
}

/// The stop's flush publishes what is pending — here an `Error` no refusal made — before its last
/// words, `Active=false` (re-gate R15; the final gate's refusals now go out apart from it, so this
/// is the flush's own proof).
#[tokio::test(flavor = "multi_thread")]
async fn the_flush_publishes_what_is_pending_before_its_last_words() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    device.reports.error(crate::wire::ErrorReport::new(crate::wire::Reason::Rejected, "pending at the stop").target(Some("t1")));
    let flushed = tokio::time::timeout(
        Duration::from_secs(5),
        flush(&shared, &client, &mut incoming, &mut rig.outputs, &mut Mode::Fixed(Fixed::default())),
    )
    .await;
    assert!(flushed.is_ok(), "the flush did not end");
    let (error, active) = (shared.topics.error(), shared.topics.active());
    assert!(broker.wait_until(Duration::from_secs(5), |b| !b.received_on(&active).is_empty()).await, "the flush never said Active=false");
    let received = broker.received();
    let error_at = received.iter().position(|r| r.topic == error && String::from_utf8_lossy(&r.payload).contains("pending at the stop"));
    let active_at = received.iter().position(|r| r.topic == active);
    assert!(error_at.is_some(), "the flush dropped what was pending");
    assert!(error_at < active_at, "what was pending went out after the last words");
}

/// The stop's flush publishes answers first: with a thousand States pending and a broker slow but
/// steady to acknowledge, a refusal and its reply still go out within the flush's bound
/// (confirming review 3: the States went first, and the answers were cut with the flush).
#[tokio::test(flavor = "multi_thread")]
async fn the_flush_publishes_answers_before_state() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    broker.delay_acks(Duration::from_millis(500));
    for t in 0..1000 {
        let values = serde_json::json!({"n": t}).as_object().unwrap().clone();
        device.reports.state(&format!("m{t:04}"), values, false);
    }
    let refusal = crate::wire::ErrorReport::new(crate::wire::Reason::Refused, "refused at the stop").target(Some("t1"));
    let reply = Outbound { topic: Some("Store/replies/flush".into()), payload: b"{}".to_vec(), correlation: None };
    device.reports.refused(vec![refusal], vec![reply]);
    let _ = tokio::time::timeout(
        Duration::from_secs(5),
        flush(&shared, &client, &mut incoming, &mut rig.outputs, &mut Mode::Fixed(Fixed::default())),
    )
    .await;
    let error = shared.topics.error();
    assert!(
        broker
            .wait_until(Duration::from_secs(1), |b| b
                .received_on(&error)
                .iter()
                .any(|r| String::from_utf8_lossy(&r.payload).contains("refused at the stop")))
            .await,
        "the refusal was not published within the flush: the States went first"
    );
    assert!(!broker.received_on("Store/replies/flush").is_empty(), "the reply was not published within the flush");
}

/// A write of level 40 held again before proof, its assumption published.
fn re_held_assumption(device: &Device) -> String {
    device.mailbox.connected();
    let topic = device.topics.desired("t1", "level");
    route(device, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), None), Instant::now());
    assert!(device.mailbox.try_take().is_some());
    device.mailbox.applied(true);
    device.reports.assume_unproven("t1", serde_json::json!({"level": 40}).as_object().unwrap().clone());
    let _ = device.mailbox.disconnected();
    topic
}

/// A live request replacing a write held again before proof ends that write, and withdraws its
/// assumption: retracted before recovery, it settles nothing, and a later request of the same
/// value meets no current value to be satisfied by (second confirming review 1: the never-proven
/// 40 became the proven value, and the later request was dropped as satisfied).
#[test]
fn a_live_replacement_of_a_re_held_write_withdraws_its_assumption() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    let topic = re_held_assumption(&device);
    route(&device, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), None), Instant::now());
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&device, &topic, &retraction, Instant::now());
    assert_eq!(
        device.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        None,
        "a live replacement left the re-held write's assumption standing, proven by the retraction"
    );
}

/// The replaced write's assumption is withdrawn in the same step as the replacement, before the
/// worker can take the replacement: a withdrawal after the worker applied it would revert the
/// replacement's own assumption (third confirming reviews, Codex 2: proven 20, re-held 40, live
/// 60 applied — and the withdrawal put the model back at 20, the device at 60).
#[test]
fn a_replacements_withdrawal_never_reverts_the_replacement() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    device.reports.state("t1", serde_json::json!({"level": 20}).as_object().unwrap().clone(), false);
    let topic = re_held_assumption(&device);
    // The worker's turn, as soon as the live request is posted: it takes the replacement, applies
    // it, and assumes it.
    AFTER_POST.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(|device: &Device| {
            if let Some(crate::mailbox::Work::Apply { .. }) = device.mailbox.try_take() {
                device.mailbox.applied(true);
                device.reports.assume_unproven("t1", serde_json::json!({"level": 60}).as_object().unwrap().clone());
            }
        }))
    });
    route(&device, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, b"60".to_vec(), None), Instant::now());
    AFTER_POST.with(|hook| hook.borrow_mut().take());
    let model = device.reports.model();
    let state = model.states.iter().find(|(t, _)| t == "t1").map(|(_, d)| crate::wire::decode_state(d).expect("a state"));
    assert_eq!(
        state.and_then(|s| s.get("level").cloned()),
        Some(serde_json::json!(60)),
        "the replaced write's withdrawal reverted the replacement the worker had applied"
    );
}

/// The same when the live replacement expires before the retraction.
#[test]
fn an_expired_live_replacement_of_a_re_held_write_withdraws_its_assumption() {
    let rig = rig_on(1);
    let (_shared, device) = (rig.shared.clone(), rig.device.clone());
    let topic = re_held_assumption(&device);
    let properties = rumqttc::v5::mqttbytes::v5::PublishProperties { message_expiry_interval: Some(1), ..Default::default() };
    route(&device, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, b"40".to_vec(), Some(properties)), Instant::now());
    std::thread::sleep(Duration::from_millis(1100));
    let mut retraction = Publish::new(topic.clone(), QoS::AtLeastOnce, Vec::<u8>::new(), None);
    retraction.retain = true;
    route(&device, &topic, &retraction, Instant::now());
    assert_eq!(
        device.reports.values_of("t1").and_then(|v| v.get("level").cloned()),
        None,
        "an expired live replacement left the re-held write's assumption standing"
    );
}

/// The flush waits until every publish it enqueued is written and acknowledged before the
/// DISCONNECT, even a batch so small the in-flight count is still zero when it checks (the second
/// confirming review's minor): with the acks held, no DISCONNECT goes out within the bound.
#[tokio::test(flavor = "multi_thread")]
async fn the_flush_waits_for_its_own_publishes_to_be_acknowledged() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    broker.hold_acks();
    device.reports.refused(
        vec![crate::wire::ErrorReport::new(crate::wire::Reason::Refused, "refused at the stop")],
        vec![Outbound { topic: Some("Store/replies/held".into()), payload: b"{}".to_vec(), correlation: None }],
    );
    let disconnected = |b: &FakeBroker| b.events().iter().any(|e| matches!(e, mqtt_test_broker::BrokerEvent::Disconnected));
    let mut fixed = Mode::Fixed(Fixed::default());
    let flushing = flush(&shared, &client, &mut incoming, &mut rig.outputs, &mut fixed);
    tokio::pin!(flushing);
    // Half a second in, the acks still held: no DISCONNECT yet.
    let early = tokio::time::timeout(Duration::from_millis(500), flushing.as_mut()).await;
    assert!(early.is_err() && !disconnected(&broker), "the DISCONNECT went out before the flush's publishes were acknowledged");
    // The ack wait's deadline spent, the DISCONNECT goes anyway, best effort (the second
    // confirming review's minor 2: none was sent).
    let _ = tokio::time::timeout(Duration::from_secs(5), flushing).await;
    assert!(
        broker.wait_until(Duration::from_secs(2), |b| disconnected(b)).await,
        "with the acks never coming, no DISCONNECT was sent at all"
    );
    broker.release_acks();
}

/// A stop that cuts a batch's publish short hands what the batch has not enqueued to the flush:
/// its answers first, nothing enqueued twice (third confirming reviews, Codex 1: the batch was
/// taken out of the reports whole, and a stop while its States backed up behind a slow broker
/// dropped its reply — owned by nobody).
#[tokio::test]
async fn a_stop_mid_batch_still_publishes_the_batchs_reply() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (stop, mut stopped) = watch::channel(false);
    let running = tokio::spawn(async move {
        let mut rig = rig;
        session(&rig.shared, &mut rig.mode, &mut rig.outputs, &mut rig.events, &mut stopped).await
    });
    let active = shared.topics.active();
    assert!(broker.wait_until(Duration::from_secs(5), |b| !b.received_on(&active).is_empty()).await, "the session never announced itself");
    // One batch (the session does not run between these): 400 States, then a reply. The broker
    // acknowledges nothing, so the States back up — rumqttc's window, then its request channel —
    // and the session waits in the batch's publish.
    broker.hold_acks();
    for t in 0..400 {
        let values = serde_json::json!({"n": t}).as_object().unwrap().clone();
        device.reports.state(&format!("m{t:04}"), values, false);
    }
    device.reports.event(Outbound { topic: Some("Store/replies/mid-batch".into()), payload: b"{}".to_vec(), correlation: None });
    assert!(broker.wait_until(Duration::from_secs(5), |b| b.held_acks() >= 10).await, "the States never filled the window");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(broker.received_on("Store/replies/mid-batch").is_empty(), "the reply went out before the States had backed up");
    // The stop cuts the batch's publish short; then the broker recovers.
    stop.send(true).expect("the session is gone");
    tokio::time::sleep(Duration::from_millis(200)).await;
    broker.release_acks();
    let ended = tokio::time::timeout(Duration::from_secs(5), running).await;
    assert!(matches!(ended, Ok(Ok(SessionEnd::Shutdown))), "the session did not end with the stop");
    assert!(
        broker.wait_until(Duration::from_secs(2), |b| !b.received_on("Store/replies/mid-batch").is_empty()).await,
        "the reply of a batch the stop cut short was never published"
    );
    // Every State once: the one whose publish the stop cut short finished, not dropped nor sent
    // again.
    let prefix = shared.topics.state("");
    let states = |b: &FakeBroker| b.received().iter().filter(|r| r.topic.starts_with(&prefix)).count();
    broker.wait_until(Duration::from_secs(2), |b| states(b) >= 400).await;
    assert_eq!(states(&broker), 400, "a State of the batch the stop cut short was lost, or published twice");
}

/// A stop that wins the session's select over a CONNACK the pump has already read still flushes:
/// the pump's own word decides whether there is a connection to flush on (third confirming
/// reviews, Claude minor 1: the session's had not taken the CONNACK yet, and skipped the flush).
#[tokio::test]
async fn a_stop_ahead_of_a_connack_the_pump_has_read_still_flushes() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, _device) = (rig.shared.clone(), rig.device.clone());
    let (stop, mut stopped) = watch::channel(false);
    let Rig { shared: ref session_shared, ref mut mode, ref mut outputs, ref mut events, .. } = rig;
    let running = session(session_shared, mode, outputs, events, &mut stopped);
    tokio::pin!(running);
    // Polled once: the pump starts, and the session waits on it. Not polled again until the stop,
    // so the CONNACK the pump reads meanwhile waits on the session's queue.
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(running.as_mut(), cx).is_pending(), "the session ended before its connection");
        std::task::Poll::Ready(())
    })
    .await;
    assert!(broker.wait_until(Duration::from_secs(5), |b| b.accepted() >= 1).await, "the broker never accepted the connection");
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop.send(true).expect("the session is gone");
    let ended = tokio::time::timeout(Duration::from_secs(5), running).await;
    assert!(matches!(ended, Ok(SessionEnd::Shutdown)), "the session did not end with the stop");
    let active = shared.topics.active();
    assert!(
        broker
            .wait_until(Duration::from_secs(1), |b| b.events().iter().any(|e| matches!(e, mqtt_test_broker::BrokerEvent::Disconnected)))
            .await,
        "the stop skipped the flush of a connection whose CONNACK the pump had read: no DISCONNECT"
    );
    assert!(!broker.received_on(&active).is_empty(), "the flush said no Active=false");
}

/// The flush's ack wait ends when the connection does: the pump's end, not only the wait's
/// deadline (third confirming reviews, Claude minor 2: a connection lost under the stop held the
/// shutdown to the ack wait's deadline). It covers a publish rumqttc refuses as it writes it — one
/// larger than the broker's maximum packet size — too: rumqttc fails the poll, and the pump ends
/// (Claude minor 4).
#[tokio::test]
async fn the_ack_wait_ends_with_the_connection() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    broker.hold_acks();
    device.reports.refused(vec![crate::wire::ErrorReport::new(crate::wire::Reason::Refused, "refused at the stop")], Vec::new());
    let started = Instant::now();
    let dropping = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        broker.drop_connection();
    };
    let mut fixed = Mode::Fixed(Fixed::default());
    let (flushed, ()) = tokio::join!(
        tokio::time::timeout(Duration::from_secs(5), flush(&shared, &client, &mut incoming, &mut rig.outputs, &mut fixed)),
        dropping
    );
    assert!(flushed.is_ok(), "the flush did not end");
    let took = started.elapsed();
    assert!(took < Duration::from_millis(800), "the ack wait outlived its connection, to its deadline: the flush took {took:?}");
}

/// The ack wait, the DISCONNECT and the linger fit in the flush's bound: with a broker that stops
/// reading — no ack, no close after the DISCONNECT — the flush ends on the linger's own bound, not
/// cut by its own (third confirming reviews, Claude minor 3: 1.5 s of ack wait and 1 s of linger
/// in a 2 s bound).
#[tokio::test]
async fn the_ack_wait_and_the_linger_fit_in_the_flushs_bound() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(5);
    let mut rig = rig_on(broker.port());
    let (shared, device) = (rig.shared.clone(), rig.device.clone());
    let (client, events) = client(&shared);
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), Duration::from_secs(10), || {});
    let first = tokio::time::timeout(Duration::from_secs(5), incoming.recv()).await.expect("no CONNACK");
    assert!(matches!(first, Some(PumpEvent::Connected)));
    let capture = crate::test_support::Capture::start();
    broker.stop_reading();
    device.reports.refused(vec![crate::wire::ErrorReport::new(crate::wire::Reason::Refused, "refused at the stop")], Vec::new());
    let flushed = tokio::time::timeout(
        Duration::from_secs(5),
        flush(&shared, &client, &mut incoming, &mut rig.outputs, &mut Mode::Fixed(Fixed::default())),
    )
    .await;
    assert!(flushed.is_ok(), "the flush did not end");
    let cut = capture.records().into_iter().filter_map(|r| r.field("message").map(str::to_owned)).find(|m| m.contains("ran out its bound"));
    assert!(cut.is_none(), "the flush ran out its bound: its ack wait and linger do not fit in it ({cut:?})");
}

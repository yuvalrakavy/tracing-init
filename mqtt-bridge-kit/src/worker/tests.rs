//! The worker's decisions that need no device: the outage conflict rule, and the outage episode.

use serde_json::json;

use super::*;
use crate::test_support::Capture;

fn request(value: Value, conflict: Conflict) -> StateValue {
    StateValue {
        context: crate::RequestContext::default(),
        target: "u1".into(),
        property: "temp".into(),
        value,
        deadline: None,
        conflict,
        check: Default::default(),
        basis: None,
        reheld: false,
    }
}

#[derive(Default)]
struct RequestMetadataDriver {
    seen: Vec<RequestContext>,
}

#[test]
fn recovery_replaces_worker_setup_and_conflict_caches_even_when_generation_is_unchanged() {
    let mailbox = Mailbox::new();
    let reports = Reports::new();
    let table = crate::TargetGenerations { instance: "instance.1".into(), targets: [("a".into(), "generation.a".into())].into() };
    reports.install_policy(RequestPolicy::Fenced(table.clone()));
    let mut worker = Worker::new(RequestMetadataDriver::default(), DriverInfo::default(), "i", mailbox.clone(), reports.clone());
    worker.configured.insert("a".into(), (RequestContext::new("generation.a").unwrap(), Some(json!({"channel":21}))));
    worker.before = Some((Instant::now(), [("a".into(), json!({"level":1}).as_object().unwrap().clone())].into()));
    worker.before_policy = Some(RequestPolicy::Fenced(table.clone()));
    let image = crate::RecoveryImage {
        context: RequestContext::default(),
        generations: table,
        states: [("a".into(), json!({"level":22}).as_object().unwrap().clone())].into(),
        configurations: [("a".into(), json!({"channel":2}))].into(),
    };
    mailbox.replace_recovery(&reports, &image, None).unwrap();
    worker.retire_cached();
    assert_eq!(worker.configured.len(), 1);
    assert_eq!(worker.configured["a"].1, Some(json!({"channel":2})), "worker retained old setup across exact recovery");
    assert_eq!(worker.before.as_ref().unwrap().1, image.states, "worker retained an old conflict basis across recovery");
    assert_eq!(worker.recovery_epoch, 1);
}
impl DeviceDriver for RequestMetadataDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo::default()
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<crate::TargetState>, OpError> {
        Ok(vec![])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        panic!("contextual apply was bypassed")
    }
    async fn apply_with_context(&mut self, _: &str, _: &PropertyMap, context: &RequestContext) -> Result<(), OpError> {
        self.seen.push(context.clone());
        Err(OpError::Rejected("device refusal".into()))
    }
    async fn execute(&mut self, _: Option<&str>, _: &crate::CommandCall) -> Result<Option<Value>, OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

#[tokio::test]
async fn batched_apply_carries_earliest_original_deadline_and_errors_keep_each_propertys_id() {
    let mailbox = Mailbox::new();
    mailbox.connected();
    let received = Instant::now();
    let first = "aa".repeat(32);
    let second = "bb".repeat(32);
    for (property, id, seconds) in [("early", &first, 10), ("late", &second, 30)] {
        let properties = rumqttc::v5::mqttbytes::v5::PublishProperties {
            message_expiry_interval: Some(seconds),
            user_properties: vec![("request-id".into(), id.clone())],
            ..Default::default()
        };
        let meta = crate::wire::request_meta(Some(&properties), received).unwrap();
        let mut value = request(json!(22), Conflict::RequestWins);
        value.property = property.into();
        value.context = meta.context;
        value.deadline = meta.deadline;
        mailbox.post_state(value, false);
    }
    let Work::Apply { target, values } = mailbox.try_take().expect("no batched work") else { panic!("not Apply") };
    assert_eq!(values.len(), 2);
    assert_eq!(
        values.last().unwrap().deadline,
        Some(received + Duration::from_secs(30)),
        "fixture must make the last request differ from the earliest expiry"
    );
    let reports = Reports::new();
    let mut worker = Worker::new(RequestMetadataDriver::default(), DriverInfo::default(), "i", mailbox, reports.clone());
    worker.write_values(&link_context().handle(), &target, values).await.unwrap();
    assert_eq!(worker.driver.seen.len(), 1);
    assert_eq!(worker.driver.seen[0].deadline(), Some(received + Duration::from_secs(10)));
    let changes = reports.take();
    assert_eq!(changes.answer_errors.len(), 2);
    let errors: BTreeMap<_, _> = changes
        .answer_errors
        .iter()
        .map(|error| {
            (
                error.property.clone().unwrap(),
                serde_json::from_slice::<Value>(&error.encode()).unwrap()["request-id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(errors, BTreeMap::from([("early".into(), first), ("late".into(), second)]));
}

/// The outage conflict rule (Store kit spec §4.5), every row: unchanged → applied; changed and
/// `DeviceWins` → superseded; changed and `RequestWins` → applied; no "before" value → applied;
/// the device already at the requested value → nothing to write; the device's value unknown →
/// applied under `RequestWins`, and under `DeviceWins` not applied on a guess (review M6).
#[test]
fn the_outage_conflict_rule() {
    let (b, c) = (json!(20), json!(18));
    for conflict in [Conflict::DeviceWins, Conflict::RequestWins] {
        assert_eq!(resolve(&request(json!(22), conflict), Some(&b), Some(&b)), Resolution::Apply, "unchanged, {conflict:?}");
        assert_eq!(resolve(&request(json!(22), conflict), Some(&c), None), Resolution::Apply, "no before value, {conflict:?}");
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), Some(&b)), Resolution::Satisfied, "already there, {conflict:?}");
    }
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), None, Some(&b)), Resolution::Apply, "current unknown, RequestWins");
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), None, Some(&b)), Resolution::Unknown, "current unknown, DeviceWins");
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), Some(&c), Some(&b)), Resolution::Superseded);
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), Some(&c), Some(&b)), Resolution::Apply);
}

/// The rule's rows, in the order they are taken (the lead's F1, after the R1 re-gate): the first
/// that holds decides. With no "before" value — a first install — nothing can have changed, so
/// the request is applied even when the device cannot say its current value (kit spec §4.5).
#[test]
fn the_conflict_rule_takes_its_rows_in_order() {
    let (b, c) = (json!(20), json!(18));
    for conflict in [Conflict::DeviceWins, Conflict::RequestWins] {
        // 1. The current value known and the requested one: nothing to write, before or not.
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), Some(&b)), Resolution::Satisfied, "row 1, {conflict:?}");
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), None), Resolution::Satisfied, "row 1 with no before, {conflict:?}");
        // 2. No before value: applied, the current value known or not.
        assert_eq!(resolve(&request(json!(22), conflict), Some(&c), None), Resolution::Apply, "row 2, {conflict:?}");
        assert_eq!(
            resolve(&request(json!(22), conflict), None, None),
            Resolution::Apply,
            "row 2: a first install was not applied because the current value is unknown, {conflict:?}"
        );
        // 5. Unchanged since before: applied.
        assert_eq!(resolve(&request(json!(22), conflict), Some(&b), Some(&b)), Resolution::Apply, "row 5, {conflict:?}");
    }
    // 3. A before value, and the current one unknown: not applied on a guess under DeviceWins.
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), None, Some(&b)), Resolution::Unknown, "row 3, DeviceWins");
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), None, Some(&b)), Resolution::Apply, "row 3, RequestWins");
    // 4. Changed since before.
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), Some(&c), Some(&b)), Resolution::Superseded, "row 4, DeviceWins");
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), Some(&c), Some(&b)), Resolution::Apply, "row 4, RequestWins");
}

/// A device outage is one episode: INFO `device_connection_lost` on its first failure, DEBUG
/// after, one WARN `device_unreachable` past its threshold, INFO `device_recovered` with its
/// counts.
#[test]
fn a_device_outage_is_one_episode() {
    let log = Capture::start();
    let mut outage = DeviceOutage::new(Duration::from_millis(50), Duration::ZERO);
    let error = LinkError::new("refused");
    outage.failed(&error);
    outage.failed(&error);
    std::thread::sleep(Duration::from_millis(60));
    outage.failed(&error);
    outage.failed(&error);
    outage.recovered(2, 1, 0);
    outage.stable();
    let kinds: Vec<(tracing::Level, String)> =
        log.records().into_iter().filter(|r| !r.kind.is_empty()).map(|r| (r.level, r.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (tracing::Level::INFO, "device_connection_lost".to_owned()),
            (tracing::Level::WARN, "device_unreachable".to_owned()),
            (tracing::Level::INFO, "device_recovered".to_owned()),
        ]
    );
    let recovered = log.of_kind("device_recovered");
    assert_eq!(
        (recovered[0].field("attempts"), recovered[0].field("applied"), recovered[0].field("refused")),
        (Some("4"), Some("2"), Some("1"))
    );
}

fn link_context() -> LinkContext {
    LinkContext::new(1, "i", Reports::new(), Arc::new(crate::link::Abandoned::default()), Arc::new(OpClock::new()))
}

struct QueuedReadDriver {
    seen: Vec<(Scope, RequestContext)>,
    legacy_reads: usize,
    observation: RequestContext,
}
impl QueuedReadDriver {
    fn new() -> Self {
        Self { seen: vec![], legacy_reads: 0, observation: RequestContext::new("observed.9").unwrap() }
    }
}
impl DeviceDriver for QueuedReadDriver {
    fn info(&self) -> DriverInfo {
        DriverInfo::default()
    }
    async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: Scope) -> Result<Vec<crate::TargetState>, OpError> {
        Ok(vec![])
    }
    async fn read_state_with_context(&mut self, _: Scope) -> Result<Vec<ContextualState>, OpError> {
        self.legacy_reads += 1;
        Ok(vec![ContextualState::new(
            crate::TargetState::new("u1", json!({"temp": 18}).as_object().unwrap().clone()),
            self.observation.clone(),
        )])
    }
    async fn read_state_with_request_context(&mut self, scope: Scope, context: &RequestContext) -> Result<Vec<ContextualState>, OpError> {
        // Exercise ownership across a separate task with no ambient OTel context.
        let owned = context.clone();
        let queued = tokio::spawn(async move {
            tokio::task::yield_now().await;
            owned
        })
        .await
        .unwrap();
        self.seen.push((scope, queued.clone()));
        Ok(vec![ContextualState::new(
            crate::TargetState::new("u1", json!({"temp": 18 + self.seen.len()}).as_object().unwrap().clone()),
            self.observation.clone().with_traceparent(queued.traceparent()),
        )])
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &crate::CommandCall) -> Result<Option<Value>, OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

#[tokio::test]
async fn queued_refresh_passes_original_context_without_restamping_observations() {
    let trace = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    for parent in [Some(trace), None] {
        let mailbox = Mailbox::new();
        mailbox.connected();
        let original = RequestContext::new("request.1").unwrap().with_traceparent(parent);
        mailbox.post_read_with_context(ReadKey::Refresh(Some("u1".into())), None, None, None, original.clone());
        let reports = Reports::new();
        let mut worker = Worker::new(QueuedReadDriver::new(), DriverInfo::default(), "i", mailbox.clone(), reports.clone());
        worker.work(&link_context().handle(), mailbox.try_take().unwrap()).await.unwrap();
        assert_eq!(worker.driver.seen.len(), 1, "queued refresh bypassed its original request context hook");
        let (scope, seen) = &worker.driver.seen[0];
        assert_eq!(scope, &Scope::Target("u1".into()));
        assert_eq!(seen.generation(), original.generation());
        assert_eq!(seen.traceparent(), parent, "queued refresh lost its original trace cause");
        assert!(seen.trace_was_captured(), "queued refresh lost authoritative trace absence");
        let states = reports.take().states;
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].2.generation(), Some("observed.9"), "request token relabeled independent observation authority");
        assert_eq!(states[0].2.traceparent(), parent);
        assert_eq!(worker.driver.legacy_reads, 0);
    }
}

#[tokio::test]
async fn conflict_read_passes_the_selected_surviving_original_context() {
    let trace = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    for parent in [Some(trace), None] {
        let mailbox = Mailbox::new();
        mailbox.connected();
        let original = RequestContext::new("request.1").unwrap().with_traceparent(parent);
        let mut desired = request(json!(22), Conflict::RequestWins);
        desired.context = original.clone();
        mailbox.post_state(desired, true);
        let Work::Apply { values, .. } = mailbox.try_take().unwrap() else { panic!("fixture did not produce real checked work") };
        assert!(values[0].check.any());
        let reports = Reports::new();
        let mut worker = Worker::new(QueuedReadDriver::new(), DriverInfo::default(), "i", mailbox, reports);
        let _ = worker.decide(&link_context().handle(), "u1", values, &BTreeMap::new()).await.unwrap();
        assert_eq!(worker.driver.seen.len(), 1, "conflict read bypassed its original request context hook");
        let (scope, seen) = &worker.driver.seen[0];
        assert_eq!(scope, &Scope::Target("u1".into()));
        assert_eq!(seen.generation(), original.generation());
        assert_eq!(seen.traceparent(), parent, "conflict read lost its selected original cause");
        assert!(seen.trace_was_captured(), "conflict read lost authoritative trace absence");
        assert_eq!(worker.driver.legacy_reads, 0);
    }
}

#[tokio::test]
async fn request_read_default_preserves_existing_contextual_override() {
    let mut driver = QueuedReadDriver::new();
    struct Legacy(QueuedReadDriver);
    impl DeviceDriver for Legacy {
        fn info(&self) -> DriverInfo {
            DriverInfo::default()
        }
        async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
            Ok(())
        }
        async fn disconnect(&mut self) {}
        async fn read_state(&mut self, _: Scope) -> Result<Vec<crate::TargetState>, OpError> {
            Ok(vec![])
        }
        async fn read_state_with_context(&mut self, scope: Scope) -> Result<Vec<ContextualState>, OpError> {
            self.0.read_state_with_context(scope).await
        }
        async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
            Ok(())
        }
        async fn execute(&mut self, _: Option<&str>, _: &crate::CommandCall) -> Result<Option<Value>, OpError> {
            Ok(None)
        }
        async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
            Ok(())
        }
    }
    driver.observation = RequestContext::new("legacy.7").unwrap();
    let mut legacy = Legacy(driver);
    let states = legacy.read_state_with_request_context(Scope::All, &RequestContext::new("request.1").unwrap()).await.unwrap();
    assert_eq!(legacy.0.legacy_reads, 1, "default request read bypassed existing contextual override");
    assert_eq!(states[0].context.generation(), Some("legacy.7"));
}

#[tokio::test]
async fn queued_refresh_preserves_wire_trace_and_absence_without_otel() {
    use mqtt_test_broker::{FakeBroker, SendOptions};
    let broker = FakeBroker::start().await;
    let running = crate::Bridge::new("RequestReads").instance("i").broker(&broker.address()).start(QueuedReadDriver::new()).unwrap();
    assert!(
        broker
            .wait_until(Duration::from_secs(3), |b| b.received_on("RequestReads/Status/i").iter().any(|r| r.payload == "connected"))
            .await
    );
    let trace = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    for (index, parent) in [Some(trace), None].into_iter().enumerate() {
        let reply = format!("Reply/read/{index}");
        let mut options = SendOptions::default().response_topic(&reply);
        if let Some(parent) = parent {
            options = options.user_property("traceparent", parent);
        }
        assert!(broker.send_with("RequestReads/Command/i", r#"{"target":"u1","command":"refresh"}"#, &options));
        assert!(broker.wait_until(Duration::from_secs(3), |b| !b.received_on(&reply).is_empty()).await);
        let expected = json!({"temp": 19 + index});
        assert!(
            broker
                .wait_until(Duration::from_secs(3), |b| b
                    .received_on("RequestReads/State/i/u1")
                    .iter()
                    .any(|r| { serde_json::from_slice::<Value>(&r.payload).ok().as_ref() == Some(&expected) }))
                .await,
            "queued refresh did not publish actual new readback"
        );
        let states = broker.received_on("RequestReads/State/i/u1");
        let state = states.iter().find(|r| serde_json::from_slice::<Value>(&r.payload).ok().as_ref() == Some(&expected)).unwrap();
        assert_eq!(state.user_property("traceparent"), parent, "queued refresh wire readback changed its original cause");
        let replies = broker.received_on(&reply);
        assert_eq!(replies[0].user_property("traceparent"), parent, "refresh reply changed its own caller cause");
    }
    assert!(running.stop().await);
}

#[tokio::test]
async fn merged_writes_use_the_latest_cause_and_each_failed_property_keeps_its_own() {
    struct Driver {
        seen: Option<String>,
        partial: bool,
    }
    impl DeviceDriver for Driver {
        fn info(&self) -> DriverInfo {
            DriverInfo::default()
        }
        async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> {
            Ok(())
        }
        async fn disconnect(&mut self) {}
        async fn read_state(&mut self, _: Scope) -> Result<Vec<crate::TargetState>, OpError> {
            Ok(vec![])
        }
        async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> {
            panic!("contextual apply required")
        }
        async fn apply_with_context(&mut self, _: &str, _: &PropertyMap, context: &RequestContext) -> Result<(), OpError> {
            self.seen = context.traceparent().map(str::to_owned);
            if self.partial {
                Err(OpError::Partial(vec![crate::PropertyFailure {
                    property: "y".into(),
                    class: FailureClass::Rejected,
                    error: "no".into(),
                }]))
            } else {
                Err(OpError::Rejected("no".into()))
            }
        }
        async fn execute(&mut self, _: Option<&str>, _: &crate::CommandCall) -> Result<Option<Value>, OpError> {
            Ok(None)
        }
        async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> {
            Ok(())
        }
    }
    let a = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    let b = "00-222233445566778899aabbccddeeff00-1234567890abcdef-01";
    for partial in [false, true] {
        let mailbox = Mailbox::new();
        mailbox.connected();
        for (property, trace) in [("x", a), ("y", b)] {
            let mut value = request(json!(1), Conflict::RequestWins);
            value.property = property.into();
            value.context = RequestContext::default().with_traceparent(Some(trace));
            mailbox.post_state(value, false);
        }
        let reports = Reports::new();
        let driver = Driver { seen: None, partial };
        let mut worker = Worker::new(driver, DriverInfo::default(), "i", mailbox.clone(), reports.clone());
        worker.work(&link_context().handle(), mailbox.try_take().unwrap()).await.unwrap();
        assert_eq!(worker.driver.seen.as_deref(), Some(b), "combined apply used an earlier request's cause");
        let changes = reports.take();
        assert_eq!(changes.answer_errors.len(), if partial { 1 } else { 2 });
        for error in changes.answer_errors {
            let expected = if error.property.as_deref() == Some("x") { a } else { b };
            assert_eq!(error.context.traceparent(), Some(expected), "property failure used another request's cause");
        }
    }
}

/// An operation that has completed wins over a link loss noticed at the same moment: its result is
/// what the device did, and the loss is seen at the worker's next step. Raced unbiased, a helper's
/// `link_lost` turned a completed `Ok` into a `Link` error half the time (the mqtt_ac v2 build).
/// Here the loss comes while the operation runs, in the poll that completes it.
#[tokio::test]
async fn an_operation_that_completed_wins_over_a_link_lost_at_the_same_moment() {
    for _ in 0..64 {
        let ctx = link_context();
        let handle = ctx.handle();
        let losing = handle.clone();
        let result = op(&handle, Duration::from_secs(5), "apply", async move {
            losing.link_lost("lost meanwhile");
            Ok::<u32, OpError>(7)
        })
        .await;
        assert_eq!(result, Ok(7), "a completed operation was turned into a lost link");
        assert!(handle.is_lost(), "the loss itself was forgotten");
    }
}

/// An operation that completed and a loss reported while it was pending, both ready when the
/// operation is next polled: its result wins (item 34, the unbiased race: half the time a lost link).
#[tokio::test]
async fn a_completion_and_a_loss_ready_together_keep_the_result() {
    for _ in 0..64 {
        let ctx = link_context();
        let handle = ctx.handle();
        let (done, wait) = tokio::sync::oneshot::channel::<()>();
        let operation = op(&handle, Duration::from_secs(5), "apply", async move {
            let _ = wait.await;
            Ok::<u32, OpError>(7)
        });
        tokio::pin!(operation);
        // Polled once: admitted while the link is live, and pending.
        tokio::select! {
            biased;
            early = &mut operation => panic!("the operation ended before it was let go: {early:?}"),
            () = std::future::ready(()) => {}
        }
        handle.link_lost("lost meanwhile");
        let _ = done.send(());
        assert_eq!(operation.await, Ok(7), "a completed operation was turned into a lost link");
    }
}

/// A helper's blocking call has no deadline, even when it starts while an operation runs; the
/// operation's own blocking calls are held to its deadline (spec §4.2; the R1 re-gate's R14: the
/// deadline was the link's, so a reader's quiet read that overlapped a probe "overran" and lost
/// the link).
#[tokio::test(flavor = "multi_thread")]
async fn a_helpers_blocking_call_is_not_held_to_an_operation_it_overlapped() {
    let ctx = link_context();
    let handle = ctx.handle();
    let reader = handle.clone();
    let mut read = None;
    let probed = op(&handle, Duration::from_millis(100), "probe", async {
        read = Some(tokio::spawn(async move {
            reader
                .blocking(|| {
                    std::thread::sleep(Duration::from_millis(400));
                    "line"
                })
                .await
        }));
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok::<(), OpError>(())
    })
    .await;
    assert_eq!(probed, Ok(()));
    let read = read.expect("the helper started").await.expect("the helper ran");
    assert_eq!(read, Ok("line"), "a helper's read was held to the deadline of an operation it overlapped");
    assert!(!handle.is_lost(), "a helper's read lost the link");
    // The operation's own blocking call is held to its deadline.
    let ctx = link_context();
    let handle = ctx.handle();
    let inner = handle.clone();
    let slow = op(&handle, Duration::from_millis(100), "apply", async move {
        inner.blocking(|| std::thread::sleep(Duration::from_millis(400))).await.map_err(OpError::Link)
    })
    .await;
    assert!(matches!(slow, Err(OpError::Link(_))), "an operation's blocking call was not held to its deadline: {slow:?}");
}

/// An operation is not started on a link already lost: it is refused at once with the loss, and
/// the driver is not called (the R1 re-gate's R6: the biased wrapper ran every new operation on a
/// lost link, and catch-up writes went on after the loss).
#[tokio::test]
async fn an_operation_is_not_started_on_a_link_already_lost() {
    let ctx = link_context();
    let handle = ctx.handle();
    handle.link_lost("lost before");
    let started = std::sync::atomic::AtomicBool::new(false);
    let result = op(&handle, Duration::from_secs(5), "apply", async {
        started.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok::<u32, OpError>(7)
    })
    .await;
    assert!(
        matches!(&result, Err(OpError::Link(e)) if e.reason().contains("lost before")),
        "an operation started on a link already lost: {result:?}"
    );
    assert!(!started.load(std::sync::atomic::Ordering::SeqCst), "the driver was called on a link already lost");
}

/// JSON numbers compare by value: `24` and `24.0` are the same value, so a held request the device
/// already has is satisfied, and an unchanged "before" value is unchanged (the mqtt_ac v2 build).
#[test]
fn json_numbers_compare_by_value() {
    let (int, float) = (json!(24), json!(24.0));
    assert_eq!(
        resolve(&request(json!(24), Conflict::DeviceWins), Some(&float), Some(&json!(20))),
        Resolution::Satisfied,
        "24 and 24.0 differ"
    );
    assert_eq!(
        resolve(&request(json!(26), Conflict::DeviceWins), Some(&int), Some(&float)),
        Resolution::Apply,
        "an unchanged before value counted as a change"
    );
    // Two integers compare exactly: through f64, 2^53 + 1 and 2^53 are one (re-gate minor).
    assert_eq!(
        resolve(&request(json!(9_007_199_254_740_993_u64), Conflict::DeviceWins), Some(&json!(9_007_199_254_740_992_u64)), None),
        Resolution::Apply,
        "two different integers compared as the same value"
    );
    assert_eq!(
        resolve(&request(json!(-9_007_199_254_740_993_i64), Conflict::DeviceWins), Some(&json!(-9_007_199_254_740_992_i64)), None),
        Resolution::Apply,
        "two different negative integers compared as the same value"
    );
}

/// A device that answers the proving read and drops right after its catch-up is one outage, not an
/// INFO pair per attempt and never the WARN: recovery is declared only once a recovered link has
/// lasted (review MINOR 10; the broker side's `stable_after`).
#[test]
fn a_device_that_drops_right_after_its_catch_up_is_one_outage() {
    let log = Capture::start();
    let mut outage = DeviceOutage::new(Duration::from_millis(50), Duration::from_millis(200));
    let error = LinkError::new("dropped after the catch-up");
    outage.failed(&error);
    outage.recovered(1, 0, 0);
    outage.failed(&error);
    std::thread::sleep(Duration::from_millis(60));
    outage.recovered(0, 0, 0);
    outage.failed(&error);
    let kinds: Vec<(tracing::Level, String)> =
        log.records().into_iter().filter(|r| !r.kind.is_empty()).map(|r| (r.level, r.kind)).collect();
    assert_eq!(
        kinds,
        vec![(tracing::Level::INFO, "device_connection_lost".to_owned()), (tracing::Level::WARN, "device_unreachable".to_owned())],
        "a link lost right after its catch-up was not one outage"
    );
    // A link that lasts ends it, once, with what every catch-up of it did.
    outage.recovered(2, 0, 0);
    outage.stable();
    assert!(log.of_kind("device_recovered").is_empty(), "recovered before the link had lasted");
    std::thread::sleep(Duration::from_millis(220));
    outage.stable();
    let recovered = log.of_kind("device_recovered");
    assert!(recovered.len() == 1 && recovered[0].field("applied") == Some("3"), "{:#?}", log.records());
}

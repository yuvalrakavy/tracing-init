//! The lifecycle's pure parts: how a `Config/{instance}` is read (multi-device spec §3, §4.2), and
//! when its timers fall due.

use serde_json::json;

use super::*;
use crate::reports::Outbound;

#[test]
fn dynamic_routing_and_terminal_refusals_keep_the_ingress_cause() {
    use opentelemetry::trace::TracerProvider;
    use tracing_subscriber::prelude::*;
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
    let subscriber = tracing_subscriber::registry().with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("routing-test"))
            .with_filter(tracing_subscriber::filter::LevelFilter::INFO),
    );
    tracing::subscriber::with_default(subscriber, || {
        let mut host = lifecycle();
        let topics = Topics::new("T", "Lamp").unwrap();
        let device = Arc::new(Device {
            topics: topics.clone(),
            commands: Vec::new().into(),
            mailbox: Mailbox::new(),
            reports: Reports::new(),
            tag: Some("host".into()),
            request_lifetime_limit: None,
            span: tracing::info_span!(parent: None, "unrelated_device", instance = "Lamp"),
        });
        let mut instance = Instance::new("Lamp", topics, Want::Serve(json!({})));
        instance.rt = Some(Rt {
            id: 1,
            device: device.clone(),
            phase: Phase::Live,
            worker: true,
            read: Sub::Granted,
            setup: Sub::Granted,
            requests: Sub::Granted,
            bootstrapped: true,
            unsubscribing: 0,
        });
        host.instances.insert("Lamp".into(), instance);
        let mut outputs = Outputs::new();
        let topic = "T/Command/Lamp";
        let mut publish = Publish::new(
            topic,
            rumqttc::v5::mqttbytes::QoS::AtLeastOnce,
            br#"{"target":"t1","command":"assume","power":true}"#.to_vec(),
            None,
        );
        let (_, ingress) = session::traced(&publish, "host");
        assert!(!ingress.is_disabled(), "request carrier is disabled at INFO");
        let readback = Publish::new("T/State/Lamp/t1", rumqttc::v5::mqttbytes::QoS::AtLeastOnce, b"{}".to_vec(), None);
        assert!(session::traced(&readback, "host").1.is_disabled(), "State readback exports an INFO carrier");
        let _entered = ingress.enter();
        let expected = tracing_init::traceparent::current().expect("actual OTel ingress context");
        host.route_instance(&mut outputs, "Lamp", topic, &publish, Instant::now());
        assert_eq!(device.reports.take().states[0].2.traceparent(), Some(expected.as_str()), "routing entered an unrelated device trace");
        let terminal_cause = "00-222233445566778899aabbccddeeff00-1234567890abcdef-01";
        publish.properties = Some(rumqttc::v5::mqttbytes::v5::PublishProperties {
            response_topic: Some("Store/Event/replies".into()),
            user_properties: vec![("target-generation".into(), "invalid generation".into()), ("traceparent".into(), terminal_cause.into())],
            ..Default::default()
        });
        terminal(&device, &mut outputs, topic, &publish, "stopping");
        let answers = outputs.outbox.drain(Part::Answers);
        assert_eq!(answers.len(), 2);
        for answer in answers {
            assert_eq!(
                answer.traceparent_for_test(),
                Some(terminal_cause),
                "terminal refusal replaced a valid cause accompanying malformed generation"
            );
        }
    });
}

/// A host's session part, with no broker behind it.
fn session_part() -> Arc<session::Shared> {
    Arc::new(session::Shared {
        buffers: None,
        topics: Topics::new("T", "host").expect("topics"),
        broker_host: "127.0.0.1".into(),
        broker_port: 1,
        timing: MqttTiming::default(),
        request_lifetime_limit: None,
        request_queue_bytes: None,
        packet_limits: false,
        version: "kind test".into(),
        outage: Arc::new(crate::pump::BrokerOutage::new(Duration::from_secs(30), Duration::from_secs(5))),
        enqueued: std::sync::atomic::AtomicU64::new(0),
        ready: Arc::new(Notify::new()),
        devices: lock_order::sync::Mutex::new("session-devices", Vec::new()),
    })
}

/// A lifecycle with no instance, for its timers.
fn lifecycle() -> Lifecycle {
    let shared = session_part();
    let ready = shared.ready.clone();
    let factory: Factory = Arc::new(|_: &DeviceConfig| Err(FactoryError::Unusable("no driver in this test".into())));
    let settings = Settings {
        kind: "kind".into(),
        id: "host".into(),
        root: "T".into(),
        factory: Some(factory),
        factory_timeout: Duration::from_secs(10),
        retry: Duration::from_secs(5),
        overrides: TimingOverrides::default(),
        device_stable: None,
        mqtt: MqttTiming::default(),
    };
    let (spawner, _, _) = Spawner::new();
    Lifecycle::new(settings, shared, spawner, Span::none(), ready)
}

/// A device runtime that leaves the rotation (multi-device spec §4.4 step 3) leaves its answers and
/// unsolicited reports — refusals of deliveries that reached it as it unsubscribed — in the
/// session's outbox, not with its runtime; with no connection they are dropped. (Which reports are
/// still in the slot at that instant is a race end to end, so the conformance scenario cannot see
/// this; here it is direct.)
#[test]
fn a_retired_runtimes_answers_go_to_the_outbox() {
    let shared = session_part();
    let device = || {
        Arc::new(Device {
            topics: Topics::new("T", "Lamp").expect("topics"),
            commands: Vec::new().into(),
            mailbox: Mailbox::new(),
            reports: Reports::with_ready(shared.ready.clone()),
            tag: Some("host".into()),
            request_lifetime_limit: None,
            span: Span::none(),
        })
    };
    let answered = |device: &Device| {
        let reply = Outbound {
            context: crate::RequestContext::default(),
            topic: Some("Store/replies/r".into()),
            payload: b"{}".to_vec(),
            correlation: None,
        };
        device.reports.refused(Vec::new(), vec![reply]);
        device.reports.event(Outbound {
            context: crate::RequestContext::default(),
            topic: None,
            payload: br#"{"kind":"x"}"#.to_vec(),
            correlation: None,
        });
    };
    let mut outputs = Outputs::new();
    let live = device();
    outputs.add(&shared, 7, live.clone());
    answered(&live);
    retire_slot(&shared, &mut outputs, 7, "Lamp", true);
    assert!(outputs.slots.is_empty(), "the retired runtime stayed in the rotation");
    assert_eq!(outputs.outbox.len(), 2, "a retired runtime's answer and unsolicited report went with it");

    let mut outputs = Outputs::new();
    let offline = device();
    outputs.add(&shared, 8, offline.clone());
    answered(&offline);
    retire_slot(&shared, &mut outputs, 8, "Lamp", false);
    assert_eq!(outputs.outbox.len(), 0, "with no connection, nothing waits for the next session's model");
}

/// An instance acquired again while its sweep's retractions still wait in the outbox: they are
/// dropped with the cleanup they belong to (multi-device spec §4.5: the owner's own healing covers a
/// cleanup still pending) — sent after the new owner's read-back was subscribed, its replay would
/// read the stale own-tagged value as a "before" value and then see it cleared (the R2 confirming
/// reviews, Claude minor 3). Another instance's retractions stay.
#[tokio::test]
async fn an_instance_acquired_again_drops_its_queued_sweep_retractions() {
    let mut host = lifecycle();
    let mut outputs = Outputs::new();
    for name in ["Lamp", "Other"] {
        let seen = [(format!("T/State/{name}/t1"), Some("host".to_owned())), (format!("T/Status/{name}"), Some("host".to_owned()))];
        host.sweeps.insert(name.to_owned(), Sweep { seen: seen.into_iter().collect(), closing: 0 });
        host.sweep_done(&mut outputs, name);
    }
    assert_eq!(outputs.outbox.len(), 4, "the sweeps queued no retractions, so this test proves nothing");
    let config = json!({"bridge": "kind", "via": "host"}).to_string();
    let publish = Publish::new("T/Config/Lamp", QoS::AtLeastOnce, config, None);
    host.config(None, &mut outputs, "Lamp", &publish).await.expect("the Config is handled");
    assert!(host.instances.contains_key("Lamp"), "the instance was not acquired, so this test proves nothing");
    assert_eq!(
        outputs.outbox.len(),
        2,
        "an instance acquired again kept its sweep's retractions queued: they would clear its slot after its read-back"
    );
}

/// The replay's fallback is a timer only until it passes: past it, with a grant still pending,
/// `Active=true` waits for that grant's SUBACK — a wake at the past instant would have the session
/// spin until then.
#[test]
fn a_passed_fallback_is_no_timer() {
    let mut host = lifecycle();
    host.conn.connected = true;
    host.conn.configs_granted = Some(Instant::now() - REPLAY_FALLBACK - Duration::from_secs(1));
    assert_eq!(host.next_wake(), None, "a passed fallback woke the session again and again");
    host.conn.configs_granted = Some(Instant::now());
    assert!(host.next_wake().is_some_and(|at| at > Instant::now()), "the fallback to come was no timer");
}

fn read(config: serde_json::Value) -> Verdict {
    classify(config.to_string().as_bytes(), "gc", "gc-lan1")
}

struct StartFails;
impl Startable for StartFails {
    fn describe(&self) -> DriverDescription {
        DriverDescription { info: crate::DriverInfo::default(), policy: Box::new(crate::RequestPolicy::Unfenced) }
    }
    fn start(
        self: Box<Self>,
        _: crate::DriverInfo,
        _: &str,
        _: Arc<Mailbox>,
        _: Arc<Reports>,
        _: tokio::sync::watch::Receiver<bool>,
        _: Span,
    ) -> std::io::Result<crate::devices::Started> {
        Err(std::io::Error::other("injected worker thread creation failure"))
    }
}
struct RetryDriver;
impl crate::DeviceDriver for RetryDriver {
    fn info(&self) -> crate::DriverInfo {
        crate::DriverInfo::default()
    }
    async fn connect(&mut self, _: &mut crate::LinkContext) -> Result<(), crate::LinkError> {
        Ok(())
    }
    async fn disconnect(&mut self) {}
    async fn read_state(&mut self, _: crate::Scope) -> Result<Vec<crate::TargetState>, crate::OpError> {
        Ok(Vec::new())
    }
    async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), crate::OpError> {
        Ok(())
    }
    async fn execute(&mut self, _: Option<&str>, _: &crate::CommandCall) -> Result<Option<Value>, crate::OpError> {
        Ok(None)
    }
    async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), crate::OpError> {
        Ok(())
    }
}

#[tokio::test]
async fn failed_worker_start_can_retry_without_reinstalling_mapping_authority() {
    let mut host = lifecycle();
    let mut outputs = Outputs::new();
    let config = json!({"bridge":"kind","via":"host"});
    host.instances.insert("Lamp".into(), Instance::new("Lamp", Topics::new("T", "Lamp").unwrap(), Want::Serve(config.clone())));
    host.create_rt(None, &mut outputs, "Lamp", Phase::Live, LinkStatus::Connecting).await.unwrap();
    let instance = host.instances.get_mut("Lamp").unwrap();
    let id = instance.rt.as_mut().unwrap().id;
    instance.rt.as_mut().unwrap().bootstrapped = true;
    instance.ready = Some(Ready { config: config.clone(), info: StartFails.describe(), driver: Box::new(StartFails) });
    host.start_if_ready(None, &mut outputs, "Lamp").await.unwrap();
    assert!(host.instances["Lamp"].failed_config.is_some(), "worker start failure was not recorded");
    let driver: Box<dyn Startable> = Box::new(crate::devices::Built(RetryDriver));
    host.instances.get_mut("Lamp").unwrap().ready = Some(Ready { config, info: driver.describe(), driver });
    host.start_if_ready(None, &mut outputs, "Lamp").await.unwrap();
    let rt = host.instances["Lamp"].rt.as_ref().unwrap();
    let started = rt.worker && rt.id == id;
    let mut done = host.spawner.registry.close();
    tokio::time::timeout(Duration::from_secs(3), crate::devices::all_ended(&mut done)).await.expect("retried worker did not stop");
    assert!(started, "failed worker could not restart in its existing runtime");
}

/// Empty is a delete; what cannot be read names no owner.
#[test]
fn a_deleted_or_unreadable_config() {
    assert_eq!(classify(b"", "gc", "gc-lan1"), Verdict::Deleted);
    assert!(matches!(classify(b"{not json", "gc", "gc-lan1"), Verdict::Unreadable(_)));
    assert!(matches!(classify(b"[1]", "gc", "gc-lan1"), Verdict::Unreadable(_)));
    assert!(matches!(read(json!({"bridge": "gc", "via": 7})), Verdict::Unreadable(_)), "a `via` that is no string was read");
}

/// The effective owner: `via`, else the default bridge id of the kind the Config names (§4.2).
#[test]
fn the_effective_owner_is_via_or_the_kind() {
    assert_eq!(read(json!({"bridge": "gc", "via": "gc-lan2"})), Verdict::Foreign("gc-lan2".into()));
    assert_eq!(read(json!({"bridge": "gc"})), Verdict::Foreign("gc".into()), "an omitted `via` was not the kind's default id");
    assert!(matches!(classify(json!({"bridge": "gc"}).to_string().as_bytes(), "gc", "gc"), Verdict::Ours { .. }));
}

/// An omitted `via` is the default bridge id of the kind the Config itself names (`bridge`), not
/// the reading process's kind: a default `globalcache` process sharing `IRemitter` with a default
/// `rti` one ignores RTI's instance, and says no kind mismatch for it (R2 gate, Codex 9).
#[test]
fn an_omitted_via_is_the_default_id_of_the_configs_own_kind() {
    let classified = |config: serde_json::Value| classify(config.to_string().as_bytes(), "globalcache", "globalcache");
    assert_eq!(classified(json!({"bridge": "rti"})), Verdict::Foreign("rti".into()), "another kind's instance was read as this process's");
    assert!(matches!(classified(json!({"bridge": "globalcache"})), Verdict::Ours { .. }));
    assert!(
        matches!(classified(json!({"address": "a"})), Verdict::Unreadable(_)),
        "a Config naming neither a process nor a kind named this one"
    );
}

/// Config equality is on the parsed, validated value — the owner, `disabled` and the driver fields
/// (§3): `disabled: false` written out or left out, an explicit default `via` or an omitted one, is
/// no change (R2 gate, Claude minor 1).
#[test]
fn config_equality_is_on_the_owner_the_disabled_flag_and_the_driver_fields() {
    let ours = |config: serde_json::Value| match classify(config.to_string().as_bytes(), "gc", "gc") {
        Verdict::Ours { config, .. } => config,
        other => panic!("not this process's: {other:?}"),
    };
    let plain = ours(json!({"bridge": "gc", "x": 1}));
    assert_eq!(ours(json!({"bridge": "gc", "x": 1, "disabled": false})), plain, "`disabled: false` written out was a change");
    assert_eq!(ours(json!({"bridge": "gc", "via": "gc", "x": 1})), plain, "the default `via` written out was a change");
    assert_ne!(ours(json!({"bridge": "gc", "x": 1, "disabled": true})), plain);
    assert_ne!(ours(json!({"bridge": "gc", "x": 2})), plain);
}

/// Named, but another kind, or the host's own fields invalid: rejected, never served.
#[test]
fn a_config_naming_this_process_with_the_wrong_kind_or_bad_host_fields() {
    assert!(matches!(read(json!({"bridge": "rti", "via": "gc-lan1"})), Verdict::KindMismatch { kind, .. } if kind == "rti"));
    assert!(matches!(read(json!({"via": "gc-lan1"})), Verdict::Malformed { .. }), "a Config with no kind was taken");
    assert!(matches!(read(json!({"bridge": "gc", "via": "gc-lan1", "disabled": "yes"})), Verdict::Malformed { .. }));
}

/// This process's: the driver's fields apart from the host's, `disabled` read, the Config kept whole
/// for the equality that makes an identical one a no-op.
#[test]
fn this_processs_config() {
    let config = json!({"bridge": "gc", "via": "gc-lan1", "address": "10.0.0.9", "disabled": true});
    match read(config.clone()) {
        Verdict::Ours { config: kept, disabled, fields } => {
            assert_eq!(kept, config);
            assert!(disabled);
            assert_eq!(serde_json::Value::Object(fields), json!({"address": "10.0.0.9"}));
        }
        other => panic!("not this process's: {other:?}"),
    }
    assert!(matches!(read(json!({"bridge": "gc", "via": "gc-lan1"})), Verdict::Ours { disabled: false, .. }));
    // Equality is on the parsed value: the order of the fields is no change.
    let (a, b) = (br#"{"bridge":"gc","via":"gc-lan1","x":1}"#, br#"{"x":1,"via":"gc-lan1","bridge":"gc"}"#);
    let (Verdict::Ours { config: a, .. }, Verdict::Ours { config: b, .. }) = (classify(a, "gc", "gc-lan1"), classify(b, "gc", "gc-lan1"))
    else {
        panic!("not this process's");
    };
    assert_eq!(a, b, "the same Config written in another order was a change");
}

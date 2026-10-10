//! The lifecycle's pure parts: how a `Config/{instance}` is read (multi-device spec §3, §4.2), and
//! when its timers fall due.

use serde_json::json;

use super::*;
use crate::reports::Outbound;

/// A host's session part, with no broker behind it.
fn session_part() -> Arc<session::Shared> {
    Arc::new(session::Shared {
        topics: Topics::new("T", "host").expect("topics"),
        broker_host: "127.0.0.1".into(),
        broker_port: 1,
        timing: MqttTiming::default(),
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
        factory,
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
            span: Span::none(),
        })
    };
    let answered = |device: &Device| {
        let reply = Outbound { topic: Some("Store/replies/r".into()), payload: b"{}".to_vec(), correlation: None };
        device.reports.refused(Vec::new(), vec![reply]);
        device.reports.event(Outbound { topic: None, payload: br#"{"kind":"x"}"#.to_vec(), correlation: None });
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

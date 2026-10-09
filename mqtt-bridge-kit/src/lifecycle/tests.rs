//! The lifecycle's pure parts: how a `Config/{instance}` is read (multi-device spec §3, §4.2).

use serde_json::json;

use super::*;

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

/// The effective owner: `via`, else the kind's default bridge id — the kind (§4.2).
#[test]
fn the_effective_owner_is_via_or_the_kind() {
    assert_eq!(read(json!({"bridge": "gc", "via": "gc-lan2"})), Verdict::Foreign("gc-lan2".into()));
    assert_eq!(read(json!({"bridge": "gc"})), Verdict::Foreign("gc".into()), "an omitted `via` was not the kind's default id");
    assert!(matches!(classify(json!({"bridge": "gc"}).to_string().as_bytes(), "gc", "gc"), Verdict::Ours { .. }));
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

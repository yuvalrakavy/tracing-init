use super::*;
use serde_json::json;

fn value(property: &str, bytes: usize) -> StateValue {
    StateValue {
        context: RequestContext::default(),
        target: "a".into(),
        property: property.into(),
        value: json!("x".repeat(bytes)),
        deadline: None,
        conflict: Conflict::RequestWins,
        check: Check::default(),
        basis: None,
        reheld: false,
    }
}
fn command(bytes: usize) -> CommandCall {
    CommandCall { command: "run".into(), params: json!({"data":"x".repeat(bytes)}).as_object().unwrap().clone() }
}

#[test]
fn oversized_property_and_target_updates_preserve_the_old_queue_and_copy_state() {
    let mailbox = Mailbox::with_byte_limit(Some(8192));
    mailbox.connected();
    assert_eq!(mailbox.post_target(vec![value("level", 64)]), Posted::Queued);
    let mut withdrawals = 0;
    assert!(
        matches!(mailbox.post_target_then(vec![value("level", 16384), value("other", 1)], |_, _| withdrawals += 1), Posted::Refused(_)),
        "oversized target update bypassed mailbox admission"
    );
    assert_eq!(withdrawals, 0, "rejected target update withdrew an existing assumption");
    assert_eq!(mailbox.post_state(value("level", 64), false), Posted::Skipped, "rejected target update changed the old copy pin");
    assert!(
        matches!(mailbox.post_state(value("level", 16384), false), Posted::Refused(_)),
        "oversized property update bypassed mailbox admission"
    );
    let Some(Work::Apply { values, .. }) = mailbox.try_take() else { panic!("old state disappeared") };
    assert_eq!(values.len(), 1);
    assert_eq!(values[0].value, json!("x".repeat(64)), "rejected request replaced earlier pending work");
}

#[test]
fn configuration_and_inflight_command_parameters_remain_inside_the_byte_budget() {
    let mailbox = Mailbox::with_byte_limit(Some(8192));
    mailbox.connected();
    assert!(
        matches!(
            mailbox.post_config_with_context("a", Some(json!({"data":"x".repeat(16384)})), RequestContext::default()),
            Posted::Refused(_)
        ),
        "oversized config bypassed mailbox admission"
    );
    assert_eq!(mailbox.post_command(None, command(4096), None, None), Posted::Queued);
    assert!(matches!(mailbox.try_take(), Some(Work::Execute { .. })));
    assert!(
        matches!(mailbox.post_command(None, command(4096), None, None), Posted::Refused(_)),
        "taking a command released its still-active parameter budget"
    );
}

#[test]
fn coalesced_reads_cannot_accumulate_unbounded_callers_or_replace_parameters_on_refusal() {
    let mailbox = Mailbox::with_byte_limit(Some(4096));
    mailbox.connected();
    let key = ReadKey::Command { target: None, command: "read".into() };
    let mut accepted = 0;
    for _ in 0..100 {
        let reply = ReplyTo { topic: "Client/replies".into(), correlation: Some(bytes::Bytes::from(vec![1; 128])), traceparent: None };
        let call = CommandCall { command: "read".into(), ..command(32) };
        match mailbox.post_read(key.clone(), Some(call), Some(reply), None) {
            Posted::Queued => accepted += 1,
            Posted::Refused(_) => break,
            other => panic!("unexpected post result: {other:?}"),
        }
    }
    assert!(accepted > 0 && accepted < 100, "coalesced callers escaped mailbox byte admission");
    assert!(matches!(mailbox.post_read(key, Some(command(16384)), None, None), Posted::Refused(_)));
    let Some(Work::Read { call: Some(call), replies, .. }) = mailbox.try_take() else { panic!("coalesced read disappeared") };
    assert_eq!(replies.len(), accepted, "rejected read changed the original caller set");
    assert_eq!(call.params["data"], json!("x".repeat(32)), "rejected read replaced the pending parameters");
}

#[test]
fn dispatch_never_increases_the_admitted_charge_and_completion_releases_it() {
    for read in [false, true] {
        let mailbox = Mailbox::with_byte_limit(Some(8192));
        mailbox.connected();
        let posted = if read {
            mailbox.post_read(ReadKey::Command { target: None, command: "run".into() }, Some(command(4096)), None, None)
        } else {
            mailbox.post_command(None, command(4096), None, None)
        };
        assert_eq!(posted, Posted::Queued);
        let admitted = mailbox.locked().request_bytes();
        assert!(mailbox.try_take().is_some());
        assert!(mailbox.locked().request_bytes() <= admitted, "dispatch increased the admitted request charge");
        assert!(
            matches!(mailbox.post_command(None, command(4096), None, None), Posted::Refused(Refusal::ByteLimit)),
            "active read or command parameters escaped admission"
        );
        assert!(mailbox.command_done());
        assert_eq!(mailbox.post_command(None, command(4096), None, None), Posted::Queued, "completed command retained its byte charge");
    }
}

#[test]
fn refused_replacement_keeps_a_reheld_assumption_and_configuration() {
    let mailbox = Mailbox::with_byte_limit(Some(8192));
    mailbox.connected();
    mailbox.post_state(value("level", 128), false);
    mailbox.try_take().unwrap();
    mailbox.applied(true);
    mailbox.disconnected();
    mailbox.connected();
    let mut withdrawals = 0;
    assert!(matches!(mailbox.post_state_then(value("level", 16384), false, |_, _| withdrawals += 1), Posted::Refused(Refusal::ByteLimit)));
    assert_eq!(withdrawals, 0, "refused update withdrew the reheld assumption");
    let Some(Work::Apply { values, .. }) = mailbox.try_take() else { panic!("reheld value disappeared") };
    assert!(values[0].reheld);
    mailbox.applied(false);
    assert_eq!(mailbox.post_config("a", Some(json!({"data":"old"}))), Posted::Queued);
    assert!(matches!(mailbox.post_config("a", Some(json!({"data":"x".repeat(16384)}))), Posted::Refused(Refusal::ByteLimit)));
    let Some(Work::Configure { config, .. }) = mailbox.try_take() else { panic!("old configuration disappeared") };
    assert_eq!(config.unwrap()["data"], "old");
}

#[test]
fn replacements_credit_superseded_contents_and_inflight_state_keeps_its_capacity() {
    let mailbox = Mailbox::with_byte_limit(Some(8192));
    mailbox.connected();
    assert_eq!(mailbox.post_target(vec![value("level", 2048)]), Posted::Queued);
    assert_eq!(mailbox.post_target(vec![value("level", 2049)]), Posted::Queued, "replacement charged both old and new contents");
    assert!(matches!(mailbox.try_take(), Some(Work::Apply { .. })));
    assert!(
        matches!(mailbox.post_state(value("other", 4096), false), Posted::Refused(Refusal::ByteLimit)),
        "taking state released its in-flight capacity"
    );
    mailbox.failed();
    assert_eq!(mailbox.post_state(value("other", 4096), false), Posted::Queued, "failed state kept its in-flight charge");
}

#[test]
fn escaped_wire_contents_and_replay_bases_are_included_in_admission() {
    let mailbox = Mailbox::with_byte_limit(Some(8192));
    mailbox.connected();
    let mut escaped = value("escaped", 0);
    escaped.value = json!("\u{0001}".repeat(2048));
    assert!(
        matches!(mailbox.post_state(escaped, false), Posted::Refused(Refusal::ByteLimit)),
        "escaped JSON bypassed encoded-content admission"
    );
    let mut replay = value("level", 1);
    replay.basis = Some(Basis { at: Instant::now(), before: Some(json!("old".repeat(4096))) });
    assert!(
        matches!(mailbox.post_state(replay, true), Posted::Refused(Refusal::ByteLimit)),
        "large replay basis bypassed mailbox admission"
    );
    assert!(!mailbox.waiting());
    assert_eq!(mailbox.post_state(value("level", 1), false), Posted::Queued);
}

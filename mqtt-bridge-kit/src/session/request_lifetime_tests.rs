use super::*;
use crate::mailbox::{Mailbox, Work};
use serde_json::json;

fn device(limit: Option<Duration>) -> Device {
    Device {
        assume_policy: crate::AssumePolicy::Unrestricted,
        topics: Topics::new("T", "limited").unwrap(),
        commands: Vec::new().into(),
        mailbox: Mailbox::new(),
        reports: Reports::new(),
        tag: None,
        request_lifetime_limit: limit,
        span: Span::none(),
    }
}

fn property(d: &Device, expiry: Option<u32>, retained: bool, value: u32, received: Instant) {
    let topic = d.topics.desired("t1", "level");
    let properties = PublishProperties { message_expiry_interval: expiry, ..Default::default() };
    let mut publish = Publish::new(topic.clone(), QoS::AtLeastOnce, value.to_string().into_bytes(), Some(properties));
    publish.retain = retained;
    route(d, &topic, &publish, received);
}

fn take_values(d: &Device) -> Vec<StateValue> {
    match d.mailbox.try_take() {
        Some(Work::Apply { values, .. }) => values,
        other => panic!("expected apply, got {other:?}"),
    }
}

#[test]
fn request_lifetime_default_preserves_unbounded_and_long_property_requests() {
    for expiry in [None, Some(600)] {
        let d = device(None);
        d.mailbox.connected();
        let received = Instant::now();
        property(&d, expiry, false, 40, received);
        assert_eq!(take_values(&d)[0].deadline, expiry.map(|s| received + Duration::from_secs(u64::from(s))));
        assert!(d.reports.take().errors.is_empty());
    }
}

#[test]
fn request_lifetime_property_requires_bounded_wire_expiry_live_and_replayed() {
    for retained in [false, true] {
        for expiry in [None, Some(181)] {
            let d = device(Some(Duration::from_secs(180)));
            d.mailbox.connected();
            property(&d, Some(60), false, 40, Instant::now());
            property(&d, expiry, retained, 60, Instant::now());
            assert_eq!(take_values(&d)[0].value, json!(40), "invalid expiry replaced a valid request");
            let errors = d.reports.take().errors;
            assert_eq!(errors.len(), 1, "missing lifetime refusal");
            assert_eq!(errors[0].reason, Reason::Malformed);
        }
    }
}

#[test]
fn request_lifetime_property_preserves_short_receipt_deadlines_and_zero_expiry() {
    for seconds in [45, 180] {
        let d = device(Some(Duration::from_secs(180)));
        d.mailbox.connected();
        let received = Instant::now() - Duration::from_secs(20);
        property(&d, Some(seconds), false, 40, received);
        assert_eq!(take_values(&d)[0].deadline, Some(received + Duration::from_secs(u64::from(seconds))));
    }
    let d = device(Some(Duration::from_secs(180)));
    d.mailbox.connected();
    property(&d, Some(0), false, 40, Instant::now());
    assert!(d.mailbox.try_take().is_none(), "zero expiry was renewed");
}

#[test]
fn request_lifetime_target_uses_original_receipt_and_preserves_shorter_expiry() {
    for expiry in [None, Some(600), Some(45)] {
        let d = device(Some(Duration::from_secs(180)));
        d.mailbox.connected();
        let topic = d.topics.desired_target("t1");
        let received = Instant::now() - Duration::from_secs(20);
        let props = PublishProperties { message_expiry_interval: expiry, ..Default::default() };
        route(&d, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"level":40,"power":true}"#.to_vec(), Some(props)), received);
        let values = take_values(&d);
        assert_eq!(values.len(), 2);
        let expected = received + Duration::from_secs(u64::from(expiry.unwrap_or(180).min(180)));
        assert!(values.iter().all(|v| v.deadline == Some(expected)), "target values did not share bounded deadline");
    }
}

#[test]
fn request_lifetime_target_does_not_restart_after_session_delay() {
    let d = device(Some(Duration::from_secs(180)));
    d.mailbox.connected();
    let topic = d.topics.desired_target("t1");
    route(
        &d,
        &topic,
        &Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"level":40}"#.to_vec(), None),
        Instant::now() - Duration::from_secs(181),
    );
    assert!(d.mailbox.try_take().is_none(), "dispatch gave an old request a new lifetime");
}

#[test]
fn request_lifetime_expired_standard_commands_preserve_reply_context() {
    for command in ["refresh", "assume"] {
        let d = device(Some(Duration::from_secs(180)));
        d.mailbox.connected();
        let topic = d.topics.command();
        let trace = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
        let props = PublishProperties {
            response_topic: Some("Store/Event/replies".into()),
            correlation_data: Some(b"owned".to_vec().into()),
            user_properties: vec![
                ("traceparent".into(), trace.into()),
                ("target-generation".into(), "epoch.1".into()),
                ("conflict".into(), "ignored-command-value".into()),
            ],
            ..Default::default()
        };
        let payload = json!({"target":"t1","command":command,"level":40}).to_string().into_bytes();
        route(&d, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, payload, Some(props)), Instant::now() - Duration::from_secs(181));
        assert!(d.mailbox.try_take().is_none());
        let changes = d.reports.take();
        assert!(changes.states.is_empty(), "expired assume changed reported state");
        assert_eq!(changes.answer_errors.len(), 1);
        assert_eq!(changes.answer_errors[0].reason, Reason::Refused);
        assert_eq!(changes.answer_errors[0].context.generation(), Some("epoch.1"));
        assert_eq!(changes.replies.len(), 1);
        assert_eq!(changes.replies[0].correlation.as_deref(), Some(b"owned".as_slice()));
        assert_eq!(changes.replies[0].context.traceparent(), Some(trace));
        let reply: serde_json::Value = serde_json::from_slice(&changes.replies[0].payload).unwrap();
        assert_eq!(reply["reason"], "refused");
    }
}

#[test]
fn request_lifetime_commands_keep_existing_conflict_parsing_and_default_policy() {
    for limit in [None, Some(Duration::from_secs(180))] {
        let d = device(limit);
        let topic = d.topics.command();
        let props = PublishProperties { user_properties: vec![("conflict".into(), "ignored-command-value".into())], ..Default::default() };
        route(
            &d,
            &topic,
            &Publish::new(topic.clone(), QoS::AtLeastOnce, br#"{"target":"t1","command":"assume","level":40}"#.to_vec(), Some(props)),
            Instant::now(),
        );
        let changes = d.reports.take();
        assert_eq!(changes.states.len(), 1);
        assert!(changes.answer_errors.is_empty() && changes.errors.is_empty());
    }
}

#[test]
fn request_lifetime_retractions_and_configuration_do_not_require_expiry() {
    let d = device(Some(Duration::from_secs(180)));
    d.mailbox.connected();
    property(&d, Some(60), false, 40, Instant::now());
    let topic = d.topics.desired("t1", "level");
    route(&d, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, vec![], None), Instant::now());
    assert!(d.mailbox.try_take().is_none(), "expiry policy blocked an empty retraction");
    let topic = d.topics.config("t1");
    route(&d, &topic, &Publish::new(topic.clone(), QoS::AtLeastOnce, b"{}".to_vec(), None), Instant::now());
    assert!(matches!(d.mailbox.try_take(), Some(Work::Configure { .. })), "configuration acquired a request expiry requirement");
}

#[test]
fn request_lifetime_invalid_builder_limits_fail_before_managed_start() {
    for limit in [Duration::ZERO, Duration::MAX] {
        let bridge = crate::Bridge::new("T").instance("i").broker("localhost:1").request_lifetime_limit(limit);
        assert!(bridge.validate_managed_start().unwrap_err().contains("request lifetime limit"));
        let host = crate::Host::new("test").request_lifetime_limit(limit);
        assert_eq!(host.usage_exit(), Some(std::process::ExitCode::from(2)));
    }
}

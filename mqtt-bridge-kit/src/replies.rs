//! Replies (Bridge Runtime spec §5; Store kit spec §3, §11): a command's answer goes to its MQTT v5
//! response topic, carrying its correlation data, shaped as an `Event` (`"kind":"reply"`) — the
//! Store points the response topic at its `Event` lane.

use bytes::Bytes;
use rumqttc::v5::mqttbytes::v5::PublishProperties;
use serde_json::{json, Value};

use crate::topics::Topics;
use crate::wire::Reason;

/// Where a command's answer goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplyTo {
    pub topic: String,
    pub correlation: Option<Bytes>,
}

impl ReplyTo {
    /// The reply address a command's properties give, if any; an error for a response topic the
    /// bridge could never publish to (empty, or holding a wildcard: rumqttc would refuse it), and
    /// for one on the bridge's own request lanes (`Desired`, `Command`, `Config`): the reply would
    /// be read back as a request — on `Command` it carries `command`, and a momentary action ran
    /// twice (review MINOR 6).
    pub fn from_properties(properties: Option<&PublishProperties>, topics: &Topics) -> Result<Option<ReplyTo>, String> {
        let Some(topic) = properties.and_then(|p| p.response_topic.clone()) else {
            return Ok(None);
        };
        if topic.is_empty() || topic.contains(['+', '#', '\0']) {
            return Err(format!("the response topic `{topic}` is not a topic one can publish to"));
        }
        if topics.is_request_lane(&topic) {
            return Err(format!("the response topic `{topic}` is one of the bridge's own request lanes"));
        }
        Ok(Some(ReplyTo { topic, correlation: properties.and_then(|p| p.correlation_data.clone()) }))
    }
}

/// How a command ended, for its reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// It acted; with the driver's answer, if it gave one.
    Done(Option<Value>),
    /// It did not.
    Failed { reason: Reason, error: String },
}

/// The reply's payload: `{"kind":"reply", "command", "target"?, "value"?}` or, for a failure,
/// `{"kind":"reply", "command", "target"?, "error", "reason"}`.
pub fn reply_payload(target: Option<&str>, command: &str, answer: &Answer) -> Vec<u8> {
    let mut document = json!({ "kind": "reply", "command": command });
    if let Some(target) = target {
        document["target"] = Value::String(target.to_owned());
    }
    match answer {
        Answer::Done(Some(value)) => document["value"] = value.clone(),
        Answer::Done(None) => {}
        Answer::Failed { reason, error } => {
            document["error"] = Value::String(error.clone());
            document["reason"] = Value::String(reason.as_str().to_owned());
        }
    }
    document.to_string().into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_carries_the_commands_correlation_data() {
        let props = PublishProperties {
            response_topic: Some("IR/Event/living".into()),
            correlation_data: Some(Bytes::from_static(b"42")),
            ..PublishProperties::default()
        };
        let topics = Topics::new("IR", "living").unwrap();
        let to = ReplyTo::from_properties(Some(&props), &topics).unwrap().unwrap();
        assert_eq!(to, ReplyTo { topic: "IR/Event/living".into(), correlation: Some(Bytes::from_static(b"42")) });
        assert_eq!(ReplyTo::from_properties(None, &topics), Ok(None));
        let wild = PublishProperties { response_topic: Some("IR/+/x".into()), ..PublishProperties::default() };
        assert!(ReplyTo::from_properties(Some(&wild), &topics).is_err());
        let own = PublishProperties { response_topic: Some("IR/Command/living".into()), ..PublishProperties::default() };
        assert!(ReplyTo::from_properties(Some(&own), &topics).is_err(), "a reply to the bridge's own Command lane was taken");
    }

    #[test]
    fn a_reply_is_shaped_as_an_event() {
        let done: Value = serde_json::from_slice(&reply_payload(Some("tv"), "learn", &Answer::Done(Some(json!({"code": "x"}))))).unwrap();
        assert_eq!(done, json!({"kind": "reply", "command": "learn", "target": "tv", "value": {"code": "x"}}));
        let failed: Value = serde_json::from_slice(&reply_payload(
            None,
            "learn",
            &Answer::Failed { reason: Reason::Rejected, error: "no IR heard".into() },
        ))
        .unwrap();
        assert_eq!(failed, json!({"kind": "reply", "command": "learn", "error": "no IR heard", "reason": "rejected"}));
    }
}

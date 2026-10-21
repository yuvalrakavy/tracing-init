//! Each lane's payload (Store kit spec §3, §4; the JSON Schemas under `schemas/`): decoding what the
//! Store sends, encoding what the bridge publishes, and a request's MQTT v5 metadata.
//!
//! Every decoder returns a reason for a malformed payload; the session answers it with an `Error`
//! (`"reason":"malformed"`) and one INFO `validation_rejected`, never a panic.

use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::PublishProperties;
use serde_json::{json, Value};

use crate::driver::{CommandCall, PropertyMap};
use crate::generations::RequestContext;
use crate::topics::valid_segment;

/// Which change wins when a request and a change at the device both happened during one outage
/// (Store kit spec §4.5). It reaches the bridge as the request's MQTT v5 user property
/// `conflict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// The request is dropped and reported superseded (devices people operate by hand).
    DeviceWins,
    /// The request is applied (no local control; a power-on reset is not a person).
    RequestWins,
}

impl Conflict {
    /// A request without the user property: the change at the device wins, so a request never
    /// reverts what a person did by hand. (The Store always sends it.)
    pub const DEFAULT: Conflict = Conflict::DeviceWins;

    /// `DeviceWins` or `RequestWins`, as the `conflict` user property spells them.
    pub fn parse(text: &str) -> Option<Conflict> {
        match text {
            "DeviceWins" => Some(Conflict::DeviceWins),
            "RequestWins" => Some(Conflict::RequestWins),
            _ => None,
        }
    }
}

/// A request's metadata, from its MQTT v5 properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestMeta {
    /// When it expires: its message expiry, counted from its arrival at the bridge — when the pump
    /// received it, not when the session got to it. A held request is dropped then (Store kit spec
    /// Q6); one without an expiry never expires.
    pub deadline: Option<Instant>,
    /// What to do if the device changed while the request waited.
    pub conflict: Conflict,
    /// The packet's original generation, preserved through every queued form.
    pub context: RequestContext,
}

/// Decode only opaque context, leaving each lane's expiry/conflict semantics alone.
pub fn request_context(properties: Option<&PublishProperties>) -> Result<RequestContext, String> {
    let mut generations = properties.into_iter().flat_map(|p| &p.user_properties).filter(|(key, _)| key == "target-generation");
    let Some((_, generation)) = generations.next() else {
        return Ok(RequestContext::default());
    };
    if generations.next().is_some() {
        return Err("target-generation must occur exactly once".into());
    }
    RequestContext::new(generation.clone())
}

/// A request's metadata, from the properties of the publish that carried it, arriving `now`.
pub fn request_meta(properties: Option<&PublishProperties>, now: Instant) -> Result<RequestMeta, String> {
    let deadline = properties.and_then(|p| p.message_expiry_interval).map(|s| now + Duration::from_secs(u64::from(s)));
    let conflict = match properties.and_then(|p| p.user_properties.iter().find(|(k, _)| k == "conflict")) {
        None => Conflict::DEFAULT,
        Some((_, v)) => {
            Conflict::parse(v).ok_or_else(|| format!("the user property `conflict` is `{v}`, not `DeviceWins` or `RequestWins`"))?
        }
    };
    Ok(RequestMeta { deadline, conflict, context: request_context(properties)? })
}

fn parse(payload: &[u8]) -> Result<Value, String> {
    serde_json::from_slice(payload).map_err(|e| format!("not JSON: {e}"))
}

fn scalar(value: &Value) -> bool {
    !matches!(value, Value::Object(_) | Value::Array(_))
}

/// `Desired/{instance}/{target}/{property}`: a JSON scalar, or (an empty payload) a retraction.
pub fn decode_desired_property(payload: &[u8]) -> Result<Option<Value>, String> {
    if payload.is_empty() {
        return Ok(None);
    }
    let value = parse(payload)?;
    if !scalar(&value) {
        return Err("a property's requested value is a JSON scalar, not an object or an array".into());
    }
    Ok(Some(value))
}

/// `Desired/{instance}/{target}`: `{property: value, …}`, at least one, each a JSON scalar.
pub fn decode_desired_target(payload: &[u8]) -> Result<PropertyMap, String> {
    let Value::Object(values) = parse(payload)? else {
        return Err("a target's requested values are a JSON object".into());
    };
    if values.is_empty() {
        return Err("a target's requested values name no property".into());
    }
    for (property, value) in &values {
        if !valid_segment(property) {
            return Err(format!("`{property}` is not a property name (one topic segment)"));
        }
        if !scalar(value) {
            return Err(format!("the requested value of `{property}` is not a JSON scalar"));
        }
    }
    Ok(values)
}

/// `Command/{instance}`: `{target?, command, …}`; every other field is a parameter.
pub fn decode_command(payload: &[u8]) -> Result<(Option<String>, CommandCall), String> {
    let Value::Object(mut fields) = parse(payload)? else {
        return Err("a command is a JSON object".into());
    };
    let command = match fields.remove("command") {
        Some(Value::String(c)) if !c.is_empty() => c,
        _ => return Err("a command names itself in a non-empty string `command`".into()),
    };
    let target = match fields.remove("target") {
        None => None,
        Some(Value::String(t)) if valid_segment(&t) => Some(t),
        Some(_) => return Err("a command's `target` is a string, one topic segment".into()),
    };
    Ok((target, CommandCall { command, params: fields }))
}

/// The target and command a `Command` payload names, as far as they can be read — for the reply to
/// one refused as malformed; `?` for a command name that cannot be read.
pub fn command_names(payload: &[u8]) -> (Option<String>, String) {
    let fields = serde_json::from_slice::<Value>(payload).ok();
    let field = |name: &str| fields.as_ref().and_then(|f| f.get(name)).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned);
    (field("target").filter(|t| valid_segment(t)), field("command").unwrap_or_else(|| "?".to_owned()))
}

/// `Config/{instance}/{target}`: any JSON, or (an empty payload) a retraction.
pub fn decode_config(payload: &[u8]) -> Result<Option<Value>, String> {
    if payload.is_empty() {
        return Ok(None);
    }
    parse(payload).map(Some)
}

/// `State/{instance}/{target}`, as the bridge published it: the values, without `assumed`.
pub fn decode_state(payload: &[u8]) -> Result<PropertyMap, String> {
    let Value::Object(mut values) = parse(payload)? else {
        return Err("a state is a JSON object".into());
    };
    values.remove("assumed");
    Ok(values)
}

/// `State/{instance}/{target}`: the values, and `"assumed": true` when they were not read back.
pub fn encode_state(values: &PropertyMap, assumed: bool) -> Vec<u8> {
    let mut document = values.clone();
    if assumed {
        document.insert("assumed".into(), Value::Bool(true));
    } else {
        document.remove("assumed");
    }
    Value::Object(document).to_string().into_bytes()
}

/// Why a request failed, as `Error`'s `reason` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// The payload or topic could not be read.
    Malformed,
    /// A momentary command while the device is down, or past the queue's cap.
    Refused,
    /// The device refused it (`OpError::Rejected`).
    Rejected,
    /// The device's reply could not be used (`OpError::Unusable`).
    Unusable,
    /// No such command or property (`OpError::Unsupported`).
    Unsupported,
    /// A change at the device won an outage conflict (Store kit spec §4.5).
    Superseded,
    /// The link failed while a momentary command was in flight: it may or may not have acted, and
    /// is never retried.
    LinkLost,
}

impl Reason {
    /// The text of the `reason` field.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Malformed => "malformed",
            Reason::Refused => "refused",
            Reason::Rejected => "rejected",
            Reason::Unusable => "unusable",
            Reason::Unsupported => "unsupported",
            Reason::Superseded => "superseded",
            Reason::LinkLost => "link_lost",
        }
    }
}

/// An `Error` lane message: `{error, target?, property?, command?, reason}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorReport {
    /// Original request context, carried as MQTT metadata, outside the JSON grammar.
    pub context: RequestContext,
    /// The human-readable text.
    pub error: String,
    /// The target the failure concerns, if it concerns one.
    pub target: Option<String>,
    /// The property, if it concerns one.
    pub property: Option<String>,
    /// The command, if it concerns one.
    pub command: Option<String>,
    /// The machine-readable class.
    pub reason: Reason,
}

impl ErrorReport {
    /// A report with a reason and a text, naming no target.
    pub fn new(reason: Reason, error: impl Into<String>) -> ErrorReport {
        ErrorReport { context: RequestContext::default(), error: error.into(), target: None, property: None, command: None, reason }
    }

    /// Preserve the original request's mapping on its terminal error.
    pub fn context(mut self, context: &RequestContext) -> Self {
        self.context = context.clone();
        self
    }

    /// Name the target, if there is one.
    pub fn target(mut self, target: Option<&str>) -> ErrorReport {
        self.target = target.map(str::to_owned);
        self
    }

    /// Name the property.
    pub fn property(mut self, property: &str) -> ErrorReport {
        self.property = Some(property.to_owned());
        self
    }

    /// Name the command.
    pub fn command(mut self, command: &str) -> ErrorReport {
        self.command = Some(command.to_owned());
        self
    }

    /// The JSON payload of the `Error` lane message.
    pub fn encode(&self) -> Vec<u8> {
        let mut document = json!({ "error": self.error, "reason": self.reason.as_str() });
        for (key, value) in [("target", &self.target), ("property", &self.property), ("command", &self.command)] {
            if let Some(value) = value {
                document[key] = Value::String(value.clone());
            }
        }
        document.to_string().into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_property_request_is_a_scalar_or_a_retraction() {
        assert_eq!(decode_desired_property(b""), Ok(None));
        assert_eq!(decode_desired_property(b"22.5"), Ok(Some(json!(22.5))));
        assert_eq!(decode_desired_property(b"\"Cool\""), Ok(Some(json!("Cool"))));
        assert!(decode_desired_property(b"{\"a\":1}").is_err());
        assert!(decode_desired_property(b"[1]").is_err());
        assert!(decode_desired_property(b"not json").is_err());
    }

    #[test]
    fn a_target_request_is_an_object_of_scalars() {
        let values = decode_desired_target(br#"{"power":true,"temp":22}"#).unwrap();
        assert_eq!(values.len(), 2);
        assert!(decode_desired_target(b"{}").is_err());
        assert!(decode_desired_target(br#"{"a":{"b":1}}"#).is_err());
        assert!(decode_desired_target(br#"{"a/b":1}"#).is_err());
        assert!(decode_desired_target(b"true").is_err());
    }

    #[test]
    fn a_command_names_itself_and_keeps_its_parameters() {
        let (target, call) = decode_command(br#"{"target":"tv","command":"learn","key":"Netflix"}"#).unwrap();
        assert_eq!(target.as_deref(), Some("tv"));
        assert_eq!(call.command, "learn");
        assert_eq!(call.params.get("key"), Some(&json!("Netflix")));
        assert!(decode_command(br#"{"target":"tv"}"#).is_err());
        assert!(decode_command(br#"{"command":""}"#).is_err());
        assert!(decode_command(br#"{"command":"x","target":7}"#).is_err());
        assert!(decode_command(b"[]").is_err());
    }

    #[test]
    fn the_conflict_rule_and_the_deadline_come_from_the_properties() {
        let now = Instant::now();
        let props = PublishProperties {
            message_expiry_interval: Some(30),
            user_properties: vec![("conflict".into(), "RequestWins".into())],
            ..PublishProperties::default()
        };
        let meta = request_meta(Some(&props), now).unwrap();
        assert_eq!(meta.deadline, Some(now + Duration::from_secs(30)));
        assert_eq!(meta.conflict, Conflict::RequestWins);
        let none = request_meta(None, now).unwrap();
        assert_eq!((none.deadline, none.conflict), (None, Conflict::DeviceWins));
        let bad = PublishProperties { user_properties: vec![("conflict".into(), "Whoever".into())], ..PublishProperties::default() };
        assert!(request_meta(Some(&bad), now).is_err());
    }

    #[test]
    fn state_and_error_documents() {
        let mut values = PropertyMap::new();
        values.insert("power".into(), json!(true));
        let assumed: Value = serde_json::from_slice(&encode_state(&values, true)).unwrap();
        assert_eq!(assumed, json!({"power": true, "assumed": true}));
        let observed: Value = serde_json::from_slice(&encode_state(&values, false)).unwrap();
        assert_eq!(observed, json!({"power": true}));
        assert_eq!(decode_state(br#"{"power":true,"assumed":true}"#).unwrap(), values);
        let error: Value = serde_json::from_slice(
            &ErrorReport::new(Reason::Superseded, "changed at the device").target(Some("u1")).property("power").encode(),
        )
        .unwrap();
        assert_eq!(error, json!({"error": "changed at the device", "reason": "superseded", "target": "u1", "property": "power"}));
    }
}

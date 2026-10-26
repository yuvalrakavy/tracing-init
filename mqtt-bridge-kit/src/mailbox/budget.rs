//! Accounted encoded/decoded request contents, not allocator or RSS measurement.
use super::*;
use std::io::{self, Write};

#[derive(Default)]
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
fn sum(values: impl IntoIterator<Item = usize>) -> usize {
    values.into_iter().fold(0, usize::saturating_add)
}
fn context(context: &RequestContext) -> usize {
    128usize.saturating_add(crate::buffering::context_bytes(context))
}
fn decoded(value: &Value) -> usize {
    match value {
        Value::String(text) => 32usize.saturating_add(text.len()),
        Value::Array(values) => 32usize.saturating_add(sum(values.iter().map(decoded))),
        Value::Object(fields) => 32usize.saturating_add(decoded_map(fields)),
        _ => 32,
    }
}
fn value(value: &Value) -> usize {
    let mut encoded = Counter::default();
    if serde_json::to_writer(&mut encoded, value).is_err() {
        return usize::MAX;
    }
    decoded(value).max(encoded.0)
}
fn decoded_map(fields: &crate::PropertyMap) -> usize {
    sum(fields.iter().map(|(key, v)| sum([96, key.len(), decoded(v)])))
}
fn map_bytes(fields: &crate::PropertyMap) -> usize {
    let mut encoded = Counter::default();
    if serde_json::to_writer(&mut encoded, fields).is_err() {
        return usize::MAX;
    }
    decoded_map(fields).max(encoded.0)
}
pub(super) fn call_bytes(call: &CommandCall) -> usize {
    sum([64, call.command.len(), map_bytes(&call.params)])
}
fn reply(reply: &ReplyTo) -> usize {
    sum([96, reply.topic.len(), reply.correlation.as_ref().map_or(0, bytes::Bytes::len), reply.traceparent.as_ref().map_or(0, String::len)])
}
pub(super) fn command_bytes(target: Option<&str>, call: &CommandCall, answer: Option<&ReplyTo>, cause: &RequestContext) -> usize {
    sum([128, target.map_or(0, str::len), call_bytes(call), answer.map_or(0, reply), context(cause)])
}
fn state(state: &StateValue) -> usize {
    sum([
        192,
        state.target.len(),
        state.property.len(),
        value(&state.value),
        context(&state.context),
        state.basis.as_ref().map_or(0, |basis| basis.before.as_ref().map_or(32, value)),
    ])
}
fn config(target: &str, contents: Option<&Value>, cause: &RequestContext) -> usize {
    sum([96, target.len(), contents.map_or(0, value), context(cause)])
}
pub(super) fn entry(entry: &Entry) -> usize {
    match entry {
        Entry::State { value, .. } => state(value),
        Entry::Config { target, config: contents, context, .. } => config(target, contents.as_ref(), context),
        Entry::Momentary { target, call, reply, context, .. } => command_bytes(target.as_deref(), call, reply.as_ref(), context),
        Entry::Read { key, call, callers, context: cause } => {
            let key_bytes = match key {
                ReadKey::Refresh(target) => target.as_ref().map_or(0, String::len),
                ReadKey::Command { target, command } => target.as_ref().map_or(0, String::len).saturating_add(command.len()),
            };
            sum([
                128,
                key_bytes,
                context(cause),
                call.as_ref().map_or(0, call_bytes),
                sum(callers.iter().map(|caller| sum([128, context(&caller.context), caller.reply.as_ref().map_or(0, reply)]))),
            ])
        }
    }
}
impl Inner {
    pub(super) fn request_bytes(&self) -> usize {
        sum([
            256,
            sum(self.pending.iter().map(entry)),
            sum(self.in_flight.iter().map(state)),
            self.in_flight_config.as_ref().map_or(0, |(target, contents, cause)| config(target, contents.as_ref(), cause)),
            sum(self.unproven.iter().map(|(v, _)| state(v))),
            // Keep the admitted entry's entire reservation while the worker owns
            // its parameters. Dispatch must neither release nor increase it.
            self.in_flight_command.as_ref().map_or(0, |_| self.in_flight_command_bytes),
            sum(self
                .copies
                .iter()
                .map(|((target, property), (v, _, cause))| sum([96, target.len(), property.len(), value(v), context(cause)]))),
            sum(self.bases.iter().map(|((target, property), (v, basis, cause))| {
                sum([128, target.len(), property.len(), value(v), basis.before.as_ref().map_or(0, value), context(cause)])
            })),
            sum(self.abandoned.iter().map(|(target, property, cause)| sum([64, target.len(), property.len(), context(cause)]))),
        ])
    }
}
impl Mailbox {
    pub(super) fn admission_backup(&self, inner: &Inner) -> Option<Inner> {
        self.byte_limit.map(|_| inner.clone())
    }
    pub(super) fn admit(&self, inner: &mut Inner, before: Option<Inner>, posted: Posted) -> Posted {
        if let (Some(limit), Some(before)) = (self.byte_limit, before) {
            if matches!(posted, Posted::Queued | Posted::Held) && inner.request_bytes() > limit {
                *inner = before;
                return Posted::Refused(Refusal::ByteLimit);
            }
        }
        posted
    }
}

//! Opaque request generations for drivers with mutable physical inventories.
use crate::driver::{CommandCall, PropertyMap, TargetState, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub(crate) struct DriverDescription {
    pub info: crate::DriverInfo,
    pub policy: Box<RequestPolicy>,
}
impl DriverDescription {
    pub fn new(driver: &impl crate::DeviceDriver) -> Self {
        Self { info: driver.info(), policy: Box::new(driver.request_policy()) }
    }
}

/// Original request metadata. A generation is a mapping fence, not an identity.
#[derive(Debug, Clone, Default)]
pub struct RequestContext {
    target_generation: Option<String>,
    metadata: Option<std::sync::Arc<RequestMetadata>>,
}
#[derive(Debug, Clone, Default)]
struct RequestMetadata {
    parent: Option<String>,
    trace_captured: bool,
    deadline: Option<std::time::Instant>,
    request_id: Option<String>,
    observation: Option<std::sync::Arc<()>>,
}
// Expiry and diagnostic metadata do not participate in authority, mailbox
// coalescing or cache keys. Expiry and the internal observation lease are checked
// separately. The lease is never read from or written to MQTT properties.
impl PartialEq for RequestContext {
    fn eq(&self, other: &Self) -> bool {
        self.target_generation == other.target_generation
    }
}
impl Eq for RequestContext {}
impl PartialOrd for RequestContext {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for RequestContext {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.target_generation.cmp(&other.target_generation)
    }
}
impl std::hash::Hash for RequestContext {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.target_generation.hash(state);
    }
}
pub(crate) fn valid_traceparent(parent: &str) -> bool {
    parent.len() <= 512 && tracing_init::traceparent::set_remote_parent(&tracing::Span::none(), parent)
}
impl RequestContext {
    pub(crate) fn discovered(mut self, generation: &str, observation: std::sync::Arc<()>) -> Self {
        self.target_generation = Some(generation.to_owned());
        self.with_observation(observation)
    }
    pub(crate) fn has_observation(&self) -> bool {
        self.metadata.as_ref().is_some_and(|metadata| metadata.observation.is_some())
    }
    pub(crate) fn with_observation(mut self, observation: std::sync::Arc<()>) -> Self {
        self.metadata_mut().observation = Some(observation);
        self
    }
    pub(crate) fn observing(mut self, source: &Self) -> Self {
        self.metadata_mut().observation = source.metadata.as_ref().and_then(|metadata| metadata.observation.clone());
        self
    }
    pub(crate) fn observation_is_current(&self, current: &std::sync::Arc<()>) -> bool {
        self.metadata
            .as_ref()
            .and_then(|metadata| metadata.observation.as_ref())
            .map_or(true, |original| std::sync::Arc::ptr_eq(original, current))
    }
    fn metadata_mut(&mut self) -> &mut RequestMetadata {
        std::sync::Arc::make_mut(self.metadata.get_or_insert_with(Default::default))
    }
    /// Original pump-arrival expiry, carried unchanged across driver queues.
    pub fn deadline(&self) -> Option<std::time::Instant> {
        self.metadata.as_ref().and_then(|metadata| metadata.deadline)
    }
    /// Attach the original absolute expiry; never derive another TTL at a queue.
    pub fn with_deadline(mut self, deadline: Option<std::time::Instant>) -> Self {
        if self.deadline() != deadline {
            self.metadata_mut().deadline = deadline;
        }
        self
    }
    pub(crate) fn bounded_deadline(self, bound: Option<std::time::Instant>) -> Self {
        let deadline = [self.deadline(), bound].into_iter().flatten().min();
        self.with_deadline(deadline)
    }
    /// Optional canonical SHA-256 operation identity, separate from trace causality.
    pub fn request_id(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|metadata| metadata.request_id.as_deref())
    }
    /// Capture exactly 64 lowercase hexadecimal bytes, or explicit absence.
    /// Invalid identifiers are refused rather than normalized into another key.
    pub fn with_request_id(mut self, request_id: Option<&str>) -> Result<Self, String> {
        if request_id.is_some_and(|id| id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))) {
            return Err("request-id must be exactly 64 lowercase hexadecimal bytes".into());
        }
        if self.request_id() != request_id {
            self.metadata_mut().request_id = request_id.map(str::to_owned);
        }
        Ok(self)
    }
    /// Construct a checked generation, without normalizing its spelling.
    pub fn new(generation: impl Into<String>) -> Result<Self, String> {
        let generation = generation.into();
        valid_generation(&generation)?;
        Ok(Self { target_generation: Some(generation), ..Self::default() })
    }
    /// The original opaque generation, or none for an unfenced legacy request.
    pub fn generation(&self) -> Option<&str> {
        self.target_generation.as_deref()
    }
    /// Owned causal metadata, independent of the mapping fence. Invalid/absent
    /// metadata records explicit absence; it never rejects a functional request.
    pub fn with_traceparent(mut self, parent: Option<&str>) -> Self {
        let metadata = self.metadata_mut();
        metadata.parent = parent.filter(|parent| valid_traceparent(parent)).map(str::to_owned);
        metadata.trace_captured = true;
        self
    }
    /// The captured W3C cause, if valid. Context equality remains generation-only.
    pub fn traceparent(&self) -> Option<&str> {
        self.metadata.as_ref().and_then(|metadata| metadata.parent.as_deref())
    }
    pub(crate) fn trace_was_captured(&self) -> bool {
        self.metadata.as_ref().is_some_and(|metadata| metadata.trace_captured)
    }
    /// Enabled carrier for one operation crossing a queue boundary. Instrument
    /// the future with this span; never hold an enter guard over an await.
    pub fn operation_span(&self, instance: &str, operation: &str) -> tracing::Span {
        let Some(parent) = self.traceparent() else {
            return tracing::Span::none();
        };
        let span = tracing::info_span!(parent:None,"bridge_operation",instance,operation);
        tracing_init::traceparent::set_remote_parent(&span, parent);
        span
    }
}
/// Complete runtime mapping versions, supplied before a fixed bridge starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetGenerations {
    /// Generation for targetless controller commands/events.
    pub instance: String,
    /// Exact generations for all current targets, including metadata targets.
    pub targets: BTreeMap<String, String>,
}
/// Generation enforcement is optional; existing drivers remain unfenced.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum RequestPolicy {
    /// No inventory fence is required.
    #[default]
    Unfenced,
    /// Require the exact supplied mapping version for mutations.
    Fenced(TargetGenerations),
}
impl RequestPolicy {
    pub(crate) fn validate(&self, fixed: bool) -> Result<(), String> {
        if let Self::Fenced(table) = self {
            if !fixed {
                return Err("generation fencing requires a fixed Bridge; dynamic factories are unsupported".into());
            }
            table.validate()?;
        }
        Ok(())
    }
}
impl TargetGenerations {
    /// Validate the complete table before it can become a routing authority.
    pub fn validate(&self) -> Result<(), String> {
        valid_generation(&self.instance)?;
        for (target, generation) in &self.targets {
            if !crate::topics::valid_segment(target) {
                return Err(format!("invalid generation-table target `{target}`"));
            }
            valid_generation(generation)?;
        }
        Ok(())
    }
}

/// Exact retired mappings. Cleanup never uses a broad "different from next" test:
/// a fresh report may already exist when the mailbox retirement finishes.
#[derive(Debug, Default)]
pub(crate) struct RetiredGenerations {
    pub targets: BTreeMap<String, RequestContext>,
    pub instance: Option<RequestContext>,
}
impl RetiredGenerations {
    pub fn contains(&self, target: Option<&str>, context: &RequestContext) -> bool {
        match target {
            Some(target) => self.targets.get(target) == Some(context),
            None => self.instance.as_ref() == Some(context),
        }
    }
}
fn valid_generation(generation: &str) -> Result<(), String> {
    if generation.is_empty() || generation.len() > 128 || !generation.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err("target-generation must contain 1..128 visible ASCII bytes (0x21..0x7e)".into());
    }
    Ok(())
}
/// Additional pure driver validation, before conflict or cache acceptance.
#[derive(Debug, Clone, Copy)]
pub enum RequestKind<'a> {
    /// Desired properties sharing one target and context.
    Apply(&'a PropertyMap),
    /// Target configuration or its retraction.
    Configure(Option<&'a Value>),
    /// Declared command and its parameters.
    Command(&'a CommandCall),
}
/// Observed values retain the generation under which they were read.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextualState {
    /// The target and observed values.
    pub state: TargetState,
    /// Original observation context; never filled from a newer routing table.
    pub context: RequestContext,
}
impl ContextualState {
    /// Attach an original context to an observation.
    pub fn new(state: TargetState, context: RequestContext) -> Self {
        Self { state, context }
    }
}
#[cfg(test)]
mod runtime_tests;
#[cfg(test)]
mod tests;

//! What the device side reports for MQTT, handed over without waiting (lifted from mqtt_ac's
//! `reports.rs`, no-hang 3b re-review C1/C2; the fleet's rule: the device side never waits on MQTT).
//!
//! The worker, its helpers and the session hand their reports here and go on; the session
//! publishes them when the broker lets it. A bounded queue in its place made mqtt_ac's worker wait
//! on MQTT: with the broker stalled the queue filled, and the worker took no command — catch-up
//! included — until the broker recovered.
//!
//! - **Retractions:** a target removed (gone from the device) is published as an empty retained
//!   `State`, clearing it at the broker, and again after every CONNACK until it is reported again;
//!   a property removed leaves the target's `State` without it.
//! - **The retained model** — `Status`, and each target's latest `State` — is folded in at once,
//!   before anything waits, so what is published is always the latest, and a publish cut short
//!   (its session ended) loses nothing: the next session publishes the whole model again after its
//!   CONNACK ([`Reports::model`]). Between takes a target's states coalesce to its latest; one that
//!   did not change is not published again.
//! - **`Error` and `Event`** (replies included) are events, queued in order: at most
//!   [`ERROR_CAP`] and [`EVENT_CAP`], the oldest displaced past it — one WARN
//!   (`error_report_dropped`, `event_report_dropped`) when an episode of displacement starts, and an
//!   INFO (`…_drop_ended`, with how many and how long) once the session takes the queue.
//! - **The read-back:** the bridge's own retained `State` as it found it at start, for the outage
//!   conflict rule's "before" values after a restart (Store kit spec §4.5). Never published.
//! - **The broker-loss snapshot:** the model as it stood when the MQTT connection was last lost —
//!   what the bridge knew when it stopped listening, the "before" values of what the broker
//!   replays on the next connection (review B1). Until the first loss, the read-back stands for
//!   it.
//!
//! The lock is held only to change or copy what is waiting: never across an await, no other lock
//! under it, and nothing logged under it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Instant;

use bytes::Bytes;
use lock_order::sync::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::driver::PropertyMap;
use crate::generations::{RequestContext, RequestPolicy, RetiredGenerations, TargetGenerations};
use crate::topics::valid_segment;
use crate::wire::{encode_state, ErrorReport};

/// `Error` messages that may wait for the session; past it, the oldest is displaced.
pub const ERROR_CAP: usize = 64;
/// `Event` messages (replies included) that may wait for the session; past it, the oldest is
/// displaced.
pub const EVENT_CAP: usize = 64;

/// The device link, as `Status/{instance}` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    /// The worker is making a connect attempt.
    Connecting,
    /// The link is up: connected and proven by a good read or probe.
    Connected,
    /// The link is down (lost, or a connect failed), or the watchdog found the worker dead.
    Unreachable,
    /// In the Store kit's `Status` grammar, but not reported by the runtime today.
    Disabled,
}

impl LinkStatus {
    /// The text of the retained `Status` payload.
    pub fn as_str(self) -> &'static str {
        match self {
            LinkStatus::Connecting => "connecting",
            LinkStatus::Connected => "connected",
            LinkStatus::Unreachable => "unreachable",
            LinkStatus::Disabled => "disabled",
        }
    }
}

/// A message for the `Event` lane (`topic: None`) or a command's response topic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outbound {
    /// Original event or request generation, retained even after retirement.
    pub context: RequestContext,
    /// The topic, for a reply; `None` is the `Event` lane.
    pub topic: Option<String>,
    /// The JSON payload.
    pub payload: Vec<u8>,
    /// The correlation data of the command it answers.
    pub correlation: Option<Bytes>,
}

#[derive(Debug, Default)]
struct TargetModel {
    context: RequestContext,
    values: PropertyMap,
    assumed: bool,
    /// How many reports this target has had: the worker reads it around an `apply`, to tell
    /// whether the driver reported the target's state itself.
    reports: u64,
    /// For each property, the count of `reports` at its last report: a property reported during
    /// an `apply` is the device's word, newer than the write.
    seen: BTreeMap<String, u64>,
    /// Assumptions not yet proven: for each property the runtime assumed from an unacknowledged
    /// write that no probe or read has proven, the value before it (`None`: none). The conflict
    /// rule's current and "before" values use these, so an unproven write is no proof of itself
    /// (re-gate R3); the published `State` keeps the assumption.
    unproven: BTreeMap<String, Option<Value>>,
}

impl TargetModel {
    /// The values as proven: every unproven assumption replaced by the value before it.
    fn proven_values(&self) -> PropertyMap {
        let mut values = self.values.clone();
        for (property, before) in &self.unproven {
            match before {
                Some(value) => values.insert(property.clone(), value.clone()),
                None => values.remove(property),
            };
        }
        values
    }
}

/// A capped queue of events, and its episode of displacement.
#[derive(Debug)]
struct Capped<T> {
    items: VecDeque<T>,
    cap: usize,
    /// When the episode began, and how many it displaced.
    displacing: Option<(Instant, u64)>,
}

impl<T> Capped<T> {
    fn new(cap: usize) -> Capped<T> {
        Capped { items: VecDeque::new(), cap, displacing: None }
    }

    /// Queue `item`, displacing the oldest past the cap; `true` when this begins an episode.
    fn push(&mut self, item: T) -> bool {
        self.items.push_back(item);
        if self.items.len() <= self.cap {
            return false;
        }
        self.items.pop_front();
        match &mut self.displacing {
            Some((_, displaced)) => {
                *displaced += 1;
                false
            }
            None => {
                self.displacing = Some((Instant::now(), 1));
                true
            }
        }
    }
}

#[derive(Debug)]
struct State {
    request_policy: Option<RequestPolicy>,
    status: Option<LinkStatus>,
    status_changed: bool,
    targets: BTreeMap<String, TargetModel>,
    /// The targets whose state changed since the session last took them, in the order they did.
    changed: Vec<String>,
    errors: Capped<ErrorReport>,
    events: Capped<Outbound>,
    /// Answers to requests: every reply (an `Outbound` with a response topic), and every terminal
    /// `Error` of a request — uncapped, bounded by what the mailbox holds and the Store asks: the
    /// capped queues displace only unsolicited reports, never an answer (Codex final gate 2, second
    /// final gate 1, confirming review 2).
    answers: (Vec<ErrorReport>, Vec<Outbound>),
    readback: BTreeMap<String, PropertyMap>,
    /// The read-back as it stood when it was first done, complete or not: the conflict rule's
    /// "before" values, which a retained State arriving later does not change in mid-process
    /// (Claude's re-gate minor 11).
    readback_frozen: Option<BTreeMap<String, PropertyMap>>,
    /// Whether the read-back is in: 0 not yet, 1 incomplete (no SUBACK in time, or refused), 2
    /// complete. Set with the freeze, under the same lock, so a reader takes the flag and the map
    /// in one snapshot (third confirming reviews: an atomic set before the freeze, read apart from
    /// the map, said "complete" of a partial one). Nothing waits for it.
    readback_level: u8,
    /// The model when the MQTT connection was last lost, and when.
    broker_lost: Option<(Instant, BTreeMap<String, PropertyMap>)>,
    /// The names a driver reported that cannot stand as a topic segment, each said once.
    refused_names: std::collections::BTreeSet<String>,
    /// Targets removed and not reported since: their `State` is retracted, an empty retained
    /// payload, after every CONNACK too.
    removed: BTreeMap<String, RequestContext>,
    /// What an acknowledged `apply` wrote, by target, until a report gives the key: what the bridge
    /// knows for the conflict rule's snapshots, never published — the device may have clamped it.
    written: BTreeMap<String, PropertyMap>,
}

/// See the module documentation.
#[derive(Debug)]
pub struct Reports {
    mailbox: OnceLock<Weak<crate::mailbox::Mailbox>>,
    state: Mutex<State>,
    /// Told after every report; one per session — every device of a multi-device host shares it
    /// ([`with_ready`](Reports::with_ready)), so one wait serves them all.
    ready: Arc<Notify>,
    /// When the bridge started: the read-back is what the bridge knew then.
    epoch: Instant,
}

/// What changed since the session last took it.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    /// Answers to requests — every terminal `Error` of one, and every reply — past every cap, and
    /// ahead of unsolicited traffic. In normal operation they follow the batch's `Status` and
    /// `State`, so a reply follows the State its command produced (the second confirming review's
    /// F1); a stop's bounded flush publishes them first (confirming review 2, 3).
    pub answer_errors: Vec<ErrorReport>,
    /// Replies to commands, ahead of unsolicited traffic.
    pub replies: Vec<Outbound>,
    /// A changed link status.
    pub status: Option<LinkStatus>,
    /// Each changed target's `State` document; an empty one retracts it.
    pub states: Vec<(String, Vec<u8>, RequestContext)>,
    /// Unsolicited `Error` reports (malformed input among them), capped at [`ERROR_CAP`].
    pub errors: Vec<ErrorReport>,
    /// Unsolicited `Event` reports, capped at [`EVENT_CAP`].
    pub events: Vec<Outbound>,
}

/// Everything known: what the session publishes again after a CONNACK.
#[derive(Debug, Default, PartialEq)]
pub struct Model {
    /// The link status, once one has been reported.
    pub status: Option<LinkStatus>,
    /// Each target's `State` document, by target.
    pub states: Vec<(String, Vec<u8>, RequestContext)>,
}

/// What the bridge knows of every target, for the conflict rule: the reported values, and over them
/// what acknowledged writes left since.
fn known(state: &State) -> BTreeMap<String, PropertyMap> {
    let mut known: BTreeMap<String, PropertyMap> = state.targets.iter().map(|(t, m)| (t.clone(), m.proven_values())).collect();
    for (target, values) in &state.written {
        let entry = known.entry(target.clone()).or_default();
        for (property, value) in values {
            entry.insert(property.clone(), value.clone());
        }
    }
    known
}

impl Reports {
    pub(crate) fn bind_mailbox(&self, mailbox: &Arc<crate::mailbox::Mailbox>) {
        assert!(self.mailbox.set(Arc::downgrade(mailbox)).is_ok(), "reports mailbox bound twice");
    }
    pub(crate) fn context_is_current(&self, target: Option<&str>, context: &RequestContext) -> bool {
        self.locked().context_is_current(target, context)
    }
    pub(crate) fn fenced(&self) -> bool {
        matches!(self.locked().request_policy, Some(RequestPolicy::Fenced(_)))
    }
    pub(crate) fn observation_matches(&self, target: &str, observed: &RequestContext, requested: &RequestContext) -> bool {
        let state = self.locked();
        state.context_is_current(Some(target), observed)
            && (!matches!(state.request_policy, Some(RequestPolicy::Fenced(_))) || observed == requested)
    }
    pub(crate) fn install_policy(&self, policy: RequestPolicy) {
        let mut state = self.locked();
        if let Some(current) = &state.request_policy {
            assert_eq!(current, &policy, "initial installation cannot replace mapping authority");
            return;
        }
        state.request_policy = Some(policy);
    }

    /// Replace a fixed bridge's complete routing authority. The expected table
    /// is a compare-and-swap precondition, including targets that stayed unchanged.
    pub fn replace_generations(&self, expected: &TargetGenerations, next: TargetGenerations) -> Result<(), String> {
        self.replace_generations_inner(expected, next, None)
    }

    /// Preserve only the final completion of the exact active committing command.
    pub fn replace_generations_for_command(
        &self,
        expected: &TargetGenerations,
        next: TargetGenerations,
        completion: &crate::CommandCompletion,
    ) -> Result<(), String> {
        self.replace_generations_inner(expected, next, Some(completion))
    }

    fn replace_generations_inner(
        &self,
        expected: &TargetGenerations,
        next: TargetGenerations,
        completion: Option<&crate::CommandCompletion>,
    ) -> Result<(), String> {
        next.validate()?;
        let mailbox = self.mailbox.get().and_then(Weak::upgrade).ok_or("the runtime mailbox is unavailable")?;
        let unanswered = mailbox.replace_generations(self, expected, next, completion)?;
        crate::worker::report_refused(self, unanswered, "the request's device mapping was retired; refresh the complete inventory");
        self.ready.notify_one();
        Ok(())
    }

    // Only called under the mailbox lock; Reports never takes that lock itself.
    pub(crate) fn retire_generations(&self, expected: &TargetGenerations, next: TargetGenerations) -> Result<RetiredGenerations, String> {
        let mut state = self.locked();
        if state.request_policy.as_ref() != Some(&RequestPolicy::Fenced(expected.clone())) {
            return Err("the complete generation table changed or this runtime is unfenced".into());
        }
        let retired = RetiredGenerations {
            instance: (expected.instance != next.instance).then(|| RequestContext::new(&expected.instance).expect("validated table")),
            targets: expected
                .targets
                .iter()
                .filter(|(target, old)| next.targets.get(*target) != Some(*old))
                .map(|(target, old)| (target.clone(), RequestContext::new(old).expect("validated table")))
                .collect(),
        };
        state.request_policy = Some(RequestPolicy::Fenced(next));
        for (target, old) in &retired.targets {
            if state.targets.get(target).is_some_and(|model| model.context == *old) {
                state.targets.remove(target);
            }
            state.written.remove(target);
            state.readback.remove(target);
            if let Some(frozen) = &mut state.readback_frozen {
                frozen.remove(target);
            }
            if let Some((_, values)) = &mut state.broker_lost {
                values.remove(target);
            }
            state.removed.insert(target.clone(), old.clone());
            if !state.changed.contains(target) {
                state.changed.push(target.clone());
            }
        }
        Ok(retired)
    }

    pub(crate) fn current_policy(&self) -> Option<RequestPolicy> {
        self.locked().request_policy.clone()
    }
}

impl State {
    fn context_is_current(&self, target: Option<&str>, context: &RequestContext) -> bool {
        match &self.request_policy {
            None | Some(RequestPolicy::Unfenced) => true,
            Some(RequestPolicy::Fenced(table)) => {
                let expected = match target {
                    Some(target) => table.targets.get(target).map(String::as_str),
                    None => Some(table.instance.as_str()),
                };
                expected.is_some() && expected == context.generation()
            }
        }
    }
}

impl Reports {
    /// An empty model: nothing reported, no read-back.
    pub fn new() -> Arc<Reports> {
        Reports::with_ready(Arc::new(Notify::new()))
    }

    /// An empty model whose reports tell `ready` — the session's one signal, shared by every device
    /// of a multi-device host (multi-device spec §4.7).
    pub fn with_ready(ready: Arc<Notify>) -> Arc<Reports> {
        Arc::new(Reports {
            mailbox: OnceLock::new(),
            state: Mutex::new(
                "reports-lock",
                State {
                    request_policy: None,
                    status: None,
                    status_changed: false,
                    targets: BTreeMap::new(),
                    changed: Vec::new(),
                    errors: Capped::new(ERROR_CAP),
                    events: Capped::new(EVENT_CAP),
                    readback: BTreeMap::new(),
                    readback_frozen: None,
                    readback_level: 0,
                    broker_lost: None,
                    refused_names: std::collections::BTreeSet::new(),
                    answers: (Vec::new(), Vec::new()),
                    removed: BTreeMap::new(),
                    written: BTreeMap::new(),
                },
            ),
            ready,
            epoch: Instant::now(),
        })
    }

    /// The owner of a configured instance saw a foreign write to its `Status` (another process's
    /// tag, or a retraction): the current status is published again, once however many writes came
    /// — the flag coalesces (multi-device spec §4.5). Whether there was a status to reassert.
    /// Never waits.
    pub fn reassert_status(&self) -> bool {
        let reasserted = {
            let mut state = self.locked();
            state.status_changed |= state.status.is_some();
            state.status.is_some()
        };
        if reasserted {
            self.ready.notify_one();
        }
        reasserted
    }

    /// The same for one target's `State`: published again, once, if the model holds the target; a
    /// target never reported (or removed) is left as it is (multi-device spec §4.5). Whether there
    /// was a state to reassert. Never waits.
    pub fn reassert_state(&self, target: &str) -> bool {
        let reasserted = {
            let mut state = self.locked();
            let held = state.targets.contains_key(target);
            if held && !state.changed.iter().any(|t| t == target) {
                state.changed.push(target.to_owned());
            }
            held
        };
        if reasserted {
            self.ready.notify_one();
        }
        reasserted
    }

    /// Whether anything waits for the session: a changed status or state, a queued message, an
    /// answer. Never waits.
    pub fn has_untaken(&self) -> bool {
        let state = self.locked();
        state.status_changed
            || !state.changed.is_empty()
            || !state.errors.items.is_empty()
            || !state.events.items.is_empty()
            || !state.answers.0.is_empty()
            || !state.answers.1.is_empty()
    }

    /// The link status, if one has been reported.
    pub fn current_status(&self) -> Option<LinkStatus> {
        self.locked().status
    }

    fn locked(&self) -> lock_order::sync::MutexGuard<'_, State> {
        // WAIT: reports-lock
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The device link's status. Never waits.
    pub fn status(&self, status: LinkStatus) {
        let changed = {
            let mut state = self.locked();
            let changed = state.status != Some(status);
            if changed {
                state.status = Some(status);
                state.status_changed = true;
            }
            changed
        };
        if changed {
            self.ready.notify_one();
        }
    }

    /// A target's observed (or assumed) values, merged into its state. Never waits. A target or
    /// property name that cannot stand as one topic segment (empty, or holding `/`, `+`, `#`) is
    /// a driver bug: dropped, with one ERROR `driver_contract_violation` per name (review B2: one
    /// such target left the session deaf for the life of the process).
    pub fn state(&self, target: &str, values: PropertyMap, assumed: bool) {
        self.state_with_context(target, values, assumed, &RequestContext::default());
    }

    /// Merge only observations belonging to the current mapping.
    pub fn state_with_context(&self, target: &str, mut values: PropertyMap, assumed: bool, context: &RequestContext) {
        let mut refused: Vec<String> = Vec::new();
        if !valid_segment(target) {
            refused.push(format!("target `{target}`"));
            values.clear();
        }
        values.retain(|property, _| {
            let ok = valid_segment(property);
            if !ok {
                refused.push(format!("property `{property}` of `{target}`"));
            }
            ok
        });
        let (changed, newly_refused) = {
            let mut state = self.locked();
            if !state.context_is_current(Some(target), context) {
                return;
            }
            let newly_refused: Vec<String> = refused.into_iter().filter(|name| state.refused_names.insert(name.clone())).collect();
            if !valid_segment(target) {
                (false, newly_refused)
            } else {
                (Self::merge(&mut state, target, values, assumed, context), newly_refused)
            }
        };
        for name in newly_refused {
            error!(kind = "driver_contract_violation", name = %name, "A reported name cannot stand as a topic segment; dropped");
        }
        if changed {
            self.ready.notify_one();
        }
    }

    /// Retract a target gone from the device: removed from the model, its retained `State` cleared
    /// (an empty retained payload), after every CONNACK too, until it is reported again. Never
    /// waits.
    pub fn remove(&self, target: &str) {
        self.remove_with_context(target, &RequestContext::default());
    }

    /// A delayed removal cannot retract a replacement's model.
    pub fn remove_with_context(&self, target: &str, context: &RequestContext) {
        if !self.names_stand(target, None) {
            return;
        }
        {
            let mut state = self.locked();
            if !state.context_is_current(Some(target), context) {
                return;
            }
            state.targets.remove(target);
            state.written.remove(target);
            state.removed.insert(target.to_owned(), context.clone());
            if !state.changed.iter().any(|t| t == target) {
                state.changed.push(target.to_owned());
            }
        }
        self.ready.notify_one();
    }

    /// Retract one property of a target: removed from the model, and the target's `State`
    /// published without it. Never waits.
    pub fn remove_property(&self, target: &str, property: &str) {
        self.remove_property_with_context(target, property, &RequestContext::default());
    }

    /// Remove a property only from the original current mapping.
    pub fn remove_property_with_context(&self, target: &str, property: &str, context: &RequestContext) {
        if !self.names_stand(target, Some(property)) {
            return;
        }
        let changed = {
            let mut state = self.locked();
            if !state.context_is_current(Some(target), context) {
                return;
            }
            if let Some(written) = state.written.get_mut(target) {
                written.remove(property);
            }
            let removed = state.targets.get_mut(target).is_some_and(|m| m.values.remove(property).is_some());
            if removed && !state.changed.iter().any(|t| t == target) {
                state.changed.push(target.to_owned());
            }
            removed
        };
        if changed {
            self.ready.notify_one();
        }
    }

    /// Whether a target (and property) name can stand as a topic segment; one that cannot is a
    /// driver bug, said once per name.
    fn names_stand(&self, target: &str, property: Option<&str>) -> bool {
        let bad = if !valid_segment(target) {
            Some(format!("target `{target}`"))
        } else {
            property.filter(|p| !valid_segment(p)).map(|p| format!("property `{p}` of `{target}`"))
        };
        let Some(name) = bad else {
            return true;
        };
        if self.locked().refused_names.insert(name.clone()) {
            error!(kind = "driver_contract_violation", name = %name, "A reported name cannot stand as a topic segment; dropped");
        }
        false
    }

    /// Merge a validated report into the model; whether anything changed.
    fn merge(state: &mut State, target: &str, values: PropertyMap, assumed: bool, context: &RequestContext) -> bool {
        {
            state.removed.remove(target);
            let new = !state.targets.contains_key(target);
            let fenced = matches!(state.request_policy, Some(RequestPolicy::Fenced(_)));
            let model = state.targets.entry(target.to_owned()).or_default();
            let replaced = model.context != *context;
            if replaced && fenced {
                *model = TargetModel { context: context.clone(), ..TargetModel::default() };
            }
            model.context = context.clone();
            model.reports += 1;
            let mut changed = new || replaced || model.assumed != assumed;
            model.assumed = assumed;
            for (property, value) in values {
                // A report replaces what a write left of the key, and settles an unproven
                // assumption of it.
                model.seen.insert(property.clone(), model.reports);
                model.unproven.remove(&property);
                if let Some(written) = state.written.get_mut(target) {
                    written.remove(&property);
                }
                if model.values.get(&property) != Some(&value) {
                    model.values.insert(property, value);
                    changed = true;
                }
            }
            if changed && !state.changed.iter().any(|t| t == target) {
                state.changed.push(target.to_owned());
            }
            changed
        }
    }

    /// An `Error` message. Never waits: past [`ERROR_CAP`] waiting, the oldest goes.
    pub fn error(&self, report: ErrorReport) {
        let began = self.locked().errors.push(report);
        if began {
            warn!(kind = "error_report_dropped", waiting = ERROR_CAP, "Error reports are displacing older ones: MQTT is not taking them");
        }
        self.ready.notify_one();
    }

    /// An `Event` message, or a reply. Never waits: past [`EVENT_CAP`] waiting, the oldest event
    /// goes; a reply — it has a response topic — is an answer, and is never displaced.
    pub fn event(&self, message: Outbound) {
        if message.topic.is_some() {
            self.locked().answers.1.push(message);
            self.ready.notify_one();
            return;
        }
        let began = self.locked().events.push(message);
        if began {
            warn!(kind = "event_report_dropped", waiting = EVENT_CAP, "Event reports are displacing older ones: MQTT is not taking them");
        }
        self.ready.notify_one();
    }

    /// An unsolicited observation; retired generations cannot create fresh events.
    pub fn observed_event(&self, target: Option<&str>, message: Outbound) {
        let began = {
            let mut state = self.locked();
            if !state.context_is_current(target, &message.context) {
                return;
            }
            state.events.push(message)
        };
        if began {
            warn!(kind = "event_report_dropped", waiting = EVENT_CAP, "Event reports are displacing older ones: MQTT is not taking them");
        }
        self.ready.notify_one();
    }

    /// A request's terminal `Error` — its failure, a refusal, a supersession: an answer, past
    /// every cap; the capped queue keeps only unsolicited reports (confirming
    /// review 2). Never waits.
    pub fn request_error(&self, report: ErrorReport) {
        self.locked().answers.0.push(report);
        self.ready.notify_one();
    }

    /// Refused commands' `Error`s and replies: answers, past every cap. Never waits.
    pub fn refused(&self, errors: Vec<ErrorReport>, replies: Vec<Outbound>) {
        {
            let mut state = self.locked();
            state.answers.0.extend(errors);
            state.answers.1.extend(replies);
        }
        self.ready.notify_one();
    }

    /// What changed since the last take; the queued messages are taken out.
    pub fn take(&self) -> Changes {
        let (changes, errors_ended, events_ended) = {
            let mut state = self.locked();
            let status = if std::mem::take(&mut state.status_changed) { state.status } else { None };
            let states = std::mem::take(&mut state.changed)
                .into_iter()
                .filter_map(|target| match state.targets.get(&target) {
                    Some(m) => Some((target.clone(), encode_state(&m.values, m.assumed), m.context.clone())),
                    // A retraction: an empty retained payload.
                    None => state.removed.get(&target).map(|context| (target.clone(), Vec::new(), context.clone())),
                })
                .collect();
            let (answer_errors, replies) = std::mem::take(&mut state.answers);
            let errors = state.errors.items.drain(..).collect();
            let events = state.events.items.drain(..).collect();
            (
                Changes { answer_errors, replies, status, states, errors, events },
                state.errors.displacing.take(),
                state.events.displacing.take(),
            )
        };
        if let Some((since, dropped)) = errors_ended {
            info!(
                kind = "error_report_drop_ended",
                dropped,
                lasted_ms = since.elapsed().as_millis() as u64,
                "Error reports are taken again"
            );
        }
        if let Some((since, dropped)) = events_ended {
            info!(
                kind = "event_report_drop_ended",
                dropped,
                lasted_ms = since.elapsed().as_millis() as u64,
                "Event reports are taken again"
            );
        }
        changes
    }

    /// Everything known, for the session to publish again after a CONNACK; what changed is then
    /// published with it, so it is no longer waiting. Queued messages stay: they are not state.
    pub fn model(&self) -> Model {
        let mut state = self.locked();
        state.status_changed = false;
        state.changed.clear();
        let mut states: Vec<(String, Vec<u8>, RequestContext)> =
            state.targets.iter().map(|(t, m)| (t.clone(), encode_state(&m.values, m.assumed), m.context.clone())).collect();
        states.extend(state.removed.iter().map(|(t, context)| (t.clone(), Vec::new(), context.clone())));
        Model { status: state.status, states }
    }

    /// Wait for a report. Cancellation-safe: a report made meanwhile leaves a permit, and `take` is
    /// what removes it from waiting.
    pub async fn reported(&self) {
        // WAIT: reports-ready
        self.ready.notified().await;
    }

    /// Every target's values as the bridge knows them now, for the conflict rule: what was reported,
    /// and over it what an acknowledged `apply` wrote since.
    pub fn snapshot(&self) -> BTreeMap<String, PropertyMap> {
        known(&self.locked())
    }

    pub(crate) fn snapshot_with_policy(&self) -> (BTreeMap<String, PropertyMap>, Option<RequestPolicy>) {
        let state = self.locked();
        (known(&state), state.request_policy.clone())
    }

    /// An acknowledged `apply` wrote `values` to `target`: what the bridge knows for the conflict
    /// rule's later snapshots, until a report gives the key. Never published: the device may have
    /// clamped the value. A property the driver reported during the write (since its report count
    /// was `reported_before`) keeps the report. Never waits.
    pub fn written(&self, target: &str, values: &PropertyMap, reported_before: u64) {
        self.written_with_context(target, values, reported_before, &RequestContext::default());
    }

    /// Record a completed write only while its mapping remains current.
    pub fn written_with_context(&self, target: &str, values: &PropertyMap, reported_before: u64, context: &RequestContext) {
        let mut state = self.locked();
        if !state.context_is_current(Some(target), context) {
            return;
        }
        let State { targets, written, .. } = &mut *state;
        let seen = targets.get(target).map(|m| &m.seen);
        for (property, value) in values {
            if seen.and_then(|s| s.get(property)).is_some_and(|at| *at > reported_before) {
                continue;
            }
            written.entry(target.to_owned()).or_default().insert(property.clone(), value.clone());
        }
    }

    /// A target's values as the bridge knows them now — for a target with assumed feedback, what it
    /// believes the device holds, unproven assumptions left out (re-gate R3).
    pub fn values_of(&self, target: &str) -> Option<PropertyMap> {
        self.locked().targets.get(target).map(TargetModel::proven_values)
    }

    /// The runtime assumes `values` of `target` from an unacknowledged write no probe has proven
    /// yet: published as assumed `State`, but kept apart for the conflict rule until
    /// [`assumptions_proven`](Self::assumptions_proven) — an unproven write held again must not
    /// meet itself as the device's value (re-gate R3). Never waits.
    pub fn assume_unproven(&self, target: &str, values: PropertyMap) {
        self.assume_unproven_with_context(target, values, &RequestContext::default());
    }

    /// Install an assumption and its proof obligation atomically for this mapping.
    pub fn assume_unproven_with_context(&self, target: &str, values: PropertyMap, context: &RequestContext) {
        if !self.names_stand(target, None) {
            return;
        }
        let values: PropertyMap = values.into_iter().filter(|(property, _)| self.names_stand(target, Some(property))).collect();
        let changed = {
            let mut state = self.locked();
            if !state.context_is_current(Some(target), context) {
                return;
            }
            let model = state.targets.get(target);
            let before: Vec<(String, Option<Value>)> = values
                .keys()
                .map(|property| {
                    let earlier = model.and_then(|m| m.unproven.get(property).cloned());
                    let before = earlier.unwrap_or_else(|| model.and_then(|m| m.values.get(property).cloned()));
                    (property.clone(), before)
                })
                .collect();
            let changed = Self::merge(&mut state, target, values, true, context);
            if let Some(model) = state.targets.get_mut(target) {
                model.unproven.extend(before);
            }
            changed
        };
        if changed {
            self.ready.notify_one();
        }
    }

    /// The Store retracted a request of this key: it saw the assumed State and confirmed it, so the
    /// assumption stands (re-gate 3 S6). Never waits.
    pub fn assumption_settled(&self, target: &str, property: &str) {
        self.assumption_settled_with_context(target, property, &RequestContext::default());
    }

    /// Settle an assumption only under its original current mapping.
    pub fn assumption_settled_with_context(&self, target: &str, property: &str, context: &RequestContext) {
        let mut state = self.locked();
        if !state.context_is_current(Some(target), context) {
            return;
        }
        if let Some(model) = state.targets.get_mut(target) {
            model.unproven.remove(property);
        }
    }

    /// A write held again before proof left without being applied — expired, superseded, failed:
    /// the device may never have taken it, so its assumption is withdrawn — the key's value goes
    /// back to what it was before the write, and `State` says so (second final gate, Claude's
    /// minor 1: the assumed value stood, unproven, for good). Never waits.
    pub fn assumption_abandoned(&self, target: &str, property: &str) {
        self.assumption_abandoned_with_context(target, property, &RequestContext::default());
    }

    /// Expiry or failure of an old write cannot revert a new mapping's state.
    pub fn assumption_abandoned_with_context(&self, target: &str, property: &str, context: &RequestContext) {
        if self.withdraw_assumption_with_context(target, property, context) {
            info!(target, property, "A write the device never proved taken left unapplied; its assumed value is withdrawn");
        }
    }

    /// The same, said by the caller: whether there was an unproven assumption to withdraw. Logs
    /// nothing, so a caller under the mailbox's lock may call it (third confirming reviews, Codex
    /// 2). Never waits.
    pub fn withdraw_assumption(&self, target: &str, property: &str) -> bool {
        self.withdraw_assumption_with_context(target, property, &RequestContext::default())
    }

    /// Withdraw only an assumption for this still-current mapping. No logging.
    pub fn withdraw_assumption_with_context(&self, target: &str, property: &str, context: &RequestContext) -> bool {
        let reverted = {
            let mut state = self.locked();
            if !state.context_is_current(Some(target), context) {
                return false;
            }
            let reverted = match state.targets.get_mut(target) {
                Some(model) => match model.unproven.remove(property) {
                    Some(Some(before)) => {
                        model.values.insert(property.to_owned(), before);
                        true
                    }
                    Some(None) => {
                        model.values.remove(property);
                        true
                    }
                    None => false,
                },
                None => false,
            };
            if reverted && !state.changed.iter().any(|t| t == target) {
                state.changed.push(target.to_owned());
            }
            reverted
        };
        if reverted {
            self.ready.notify_one();
        }
        reverted
    }

    /// These writes were proven (a probe or read after them): their assumptions stand. Never waits.
    pub fn assumptions_proven(&self, values: &[crate::mailbox::StateValue]) {
        let mut state = self.locked();
        for value in values {
            if !state.context_is_current(Some(&value.target), &value.context) {
                continue;
            }
            if let Some(model) = state.targets.get_mut(&value.target) {
                model.unproven.remove(&value.property);
            }
        }
    }

    /// How many reports `target` has had.
    pub fn reports_for(&self, target: &str) -> u64 {
        self.locked().targets.get(target).map_or(0, |m| m.reports)
    }

    /// The MQTT connection was lost: what the bridge knows now is what it knew when it stopped
    /// listening, the "before" values of what the broker replays on the next connection. Never
    /// waits.
    pub fn broker_lost(&self) {
        let mut state = self.locked();
        let values = known(&state);
        state.broker_lost = Some((Instant::now(), values));
    }

    /// The "before" values of a replayed request, and as of when: the broker-loss snapshot, or —
    /// in the process's first session — the read-back, as of the start.
    pub fn broker_before(&self) -> (Instant, BTreeMap<String, PropertyMap>) {
        let state = self.locked();
        match &state.broker_lost {
            Some((at, values)) => (*at, values.clone()),
            None => (self.epoch, state.readback_frozen.clone().unwrap_or_else(|| state.readback.clone())),
        }
    }

    /// A replayed request's basis for `target`'s `property`, as of now ([`broker_before`]).
    ///
    /// [`broker_before`]: Self::broker_before
    pub fn broker_basis(&self, target: &str, property: &str) -> crate::mailbox::Basis {
        self.broker_basis_with_context(target, property, &RequestContext::default())
    }

    /// Capture a basis only for this mapping; a packet for a future generation
    /// must never inherit the previous occupant's conflict history.
    pub fn broker_basis_with_context(&self, target: &str, property: &str, context: &RequestContext) -> crate::mailbox::Basis {
        let state = self.locked();
        let (at, values) = match &state.broker_lost {
            Some((at, values)) => (*at, values),
            None => (self.epoch, state.readback_frozen.as_ref().unwrap_or(&state.readback)),
        };
        let before =
            state.context_is_current(Some(target), context).then(|| values.get(target).and_then(|v| v.get(property)).cloned()).flatten();
        crate::mailbox::Basis { at, before }
    }

    /// The read-back, as of the start: as it stood when it was done, for the conflict rule.
    pub fn read_back_before(&self) -> (Instant, BTreeMap<String, PropertyMap>) {
        let state = self.locked();
        (self.epoch, state.readback_frozen.clone().unwrap_or_else(|| state.readback.clone()))
    }

    /// The read-back is in: every retained `State` the broker had for the bridge has been read.
    /// Never waits.
    pub fn read_back_complete(&self) {
        self.read_back_reaches(2);
    }

    /// The read-back will not be complete for now — its SUBACK did not come in time, or the broker
    /// refused it — and what came is what there is; a late SUBACK may still complete it. Never
    /// waits.
    pub fn read_back_incomplete(&self) {
        self.read_back_reaches(1);
    }

    fn read_back_reaches(&self, level: u8) {
        let mut state = self.locked();
        if state.readback_frozen.is_none() {
            state.readback_frozen = Some(state.readback.clone());
        }
        state.readback_level = state.readback_level.max(level);
    }

    /// Whether the read-back is complete (not merely done).
    pub fn read_back_is_complete(&self) -> bool {
        self.locked().readback_level >= 2
    }

    /// The read-back as a driver reads it: whether it is complete, and the map — frozen once done —
    /// in one snapshot, under one lock. Never waits.
    pub fn read_back_view(&self) -> (bool, BTreeMap<String, PropertyMap>) {
        let state = self.locked();
        (state.readback_level >= 2, state.readback_frozen.clone().unwrap_or_else(|| state.readback.clone()))
    }

    /// A target's state as the bridge's retained `State` held it at start.
    pub fn read_back(&self, target: &str, values: PropertyMap) {
        self.read_back_with_context(target, values, &RequestContext::default());
    }

    /// Retained state is usable only for the mapping under which it was published.
    pub fn read_back_with_context(&self, target: &str, values: PropertyMap, context: &RequestContext) {
        let mut state = self.locked();
        if !state.context_is_current(Some(target), context) {
            return;
        }
        // Once frozen, it is what the conflict rule reads; a late State changes nothing.
        if state.readback_frozen.is_none() {
            state.readback.insert(target.to_owned(), values);
        }
    }

    /// Whether anything waits for the session (tests).
    #[cfg(test)]
    pub fn untaken(&self) -> bool {
        let state = self.locked();
        state.status_changed
            || !state.changed.is_empty()
            || !state.errors.items.is_empty()
            || !state.events.items.is_empty()
            || !state.answers.0.is_empty()
            || !state.answers.1.is_empty()
    }
}

#[cfg(test)]
mod tests;

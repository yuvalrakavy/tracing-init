//! What the device side reports for MQTT, handed over without waiting (lifted from mqtt_ac's
//! `reports.rs`, no-hang 3b re-review C1/C2; the fleet's rule: the device side never waits on MQTT).
//!
//! The worker, its helpers and the session hand their reports here and go on; the session
//! publishes them when the broker lets it. A bounded queue in its place made mqtt_ac's worker wait
//! on MQTT: with the broker stalled the queue filled, and the worker took no command — catch-up
//! included — until the broker recovered.
//!
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
//!
//! The lock is held only to change or copy what is waiting: never across an await, no other lock
//! under it, and nothing logged under it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use lock_order::sync::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::driver::PropertyMap;
use crate::wire::{encode_state, ErrorReport};

/// `Error` messages that may wait for the session; past it, the oldest is displaced.
pub const ERROR_CAP: usize = 64;
/// `Event` messages (replies included) that may wait for the session; past it, the oldest is
/// displaced.
pub const EVENT_CAP: usize = 64;

/// The device link, as `Status/{instance}` says it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkStatus {
    Connecting,
    Connected,
    Unreachable,
    Disabled,
}

impl LinkStatus {
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
    pub topic: Option<String>,
    pub payload: Vec<u8>,
    pub correlation: Option<Bytes>,
}

#[derive(Debug, Default)]
struct TargetModel {
    values: PropertyMap,
    assumed: bool,
    /// How many reports this target has had: the worker reads it around an `apply`, to tell
    /// whether the driver reported the target's state itself.
    reports: u64,
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
    status: Option<LinkStatus>,
    status_changed: bool,
    targets: BTreeMap<String, TargetModel>,
    /// The targets whose state changed since the session last took them, in the order they did.
    changed: Vec<String>,
    errors: Capped<ErrorReport>,
    events: Capped<Outbound>,
    readback: BTreeMap<String, PropertyMap>,
}

/// See the module documentation.
#[derive(Debug)]
pub struct Reports {
    state: Mutex<State>,
    ready: Notify,
}

/// What changed since the session last took it.
#[derive(Debug, Default, PartialEq)]
pub struct Changes {
    pub status: Option<LinkStatus>,
    /// Each changed target's `State` document.
    pub states: Vec<(String, Vec<u8>)>,
    pub errors: Vec<ErrorReport>,
    pub events: Vec<Outbound>,
}

/// Everything known: what the session publishes again after a CONNACK.
#[derive(Debug, Default, PartialEq)]
pub struct Model {
    pub status: Option<LinkStatus>,
    pub states: Vec<(String, Vec<u8>)>,
}

impl Reports {
    pub fn new() -> Arc<Reports> {
        Arc::new(Reports {
            state: Mutex::new(
                "reports-lock",
                State {
                    status: None,
                    status_changed: false,
                    targets: BTreeMap::new(),
                    changed: Vec::new(),
                    errors: Capped::new(ERROR_CAP),
                    events: Capped::new(EVENT_CAP),
                    readback: BTreeMap::new(),
                },
            ),
            ready: Notify::new(),
        })
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

    /// A target's observed (or assumed) values, merged into its state. Never waits.
    pub fn state(&self, target: &str, values: PropertyMap, assumed: bool) {
        let changed = {
            let mut state = self.locked();
            let new = !state.targets.contains_key(target);
            let model = state.targets.entry(target.to_owned()).or_default();
            model.reports += 1;
            let mut changed = new || model.assumed != assumed;
            model.assumed = assumed;
            for (property, value) in values {
                if model.values.get(&property) != Some(&value) {
                    model.values.insert(property, value);
                    changed = true;
                }
            }
            if changed && !state.changed.iter().any(|t| t == target) {
                state.changed.push(target.to_owned());
            }
            changed
        };
        if changed {
            self.ready.notify_one();
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

    /// An `Event` message, or a reply. Never waits: past [`EVENT_CAP`] waiting, the oldest goes.
    pub fn event(&self, message: Outbound) {
        let began = self.locked().events.push(message);
        if began {
            warn!(kind = "event_report_dropped", waiting = EVENT_CAP, "Event reports are displacing older ones: MQTT is not taking them");
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
                .filter_map(|target| state.targets.get(&target).map(|m| (target.clone(), encode_state(&m.values, m.assumed))))
                .collect();
            let errors = state.errors.items.drain(..).collect();
            let events = state.events.items.drain(..).collect();
            (Changes { status, states, errors, events }, state.errors.displacing.take(), state.events.displacing.take())
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
        Model { status: state.status, states: state.targets.iter().map(|(t, m)| (t.clone(), encode_state(&m.values, m.assumed))).collect() }
    }

    /// Wait for a report. Cancellation-safe: a report made meanwhile leaves a permit, and `take` is
    /// what removes it from waiting.
    pub async fn reported(&self) {
        // WAIT: reports-ready
        self.ready.notified().await;
    }

    /// Every target's values as the bridge knows them now.
    pub fn snapshot(&self) -> BTreeMap<String, PropertyMap> {
        self.locked().targets.iter().map(|(t, m)| (t.clone(), m.values.clone())).collect()
    }

    /// How many reports `target` has had.
    pub fn reports_for(&self, target: &str) -> u64 {
        self.locked().targets.get(target).map_or(0, |m| m.reports)
    }

    /// A target's state as the bridge's retained `State` held it at start.
    pub fn read_back(&self, target: &str, values: PropertyMap) {
        self.locked().readback.insert(target.to_owned(), values);
    }

    /// A property's value in the read-back, if it had one.
    pub fn read_back_value(&self, target: &str, property: &str) -> Option<Value> {
        self.locked().readback.get(target).and_then(|v| v.get(property)).cloned()
    }

    /// The whole read-back.
    pub fn read_back_all(&self) -> BTreeMap<String, PropertyMap> {
        self.locked().readback.clone()
    }

    /// Whether anything waits for the session (tests).
    #[cfg(test)]
    pub fn untaken(&self) -> bool {
        let state = self.locked();
        state.status_changed || !state.changed.is_empty() || !state.errors.items.is_empty() || !state.events.items.is_empty()
    }
}

#[cfg(test)]
mod tests;

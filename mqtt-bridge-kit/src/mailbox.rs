//! The device's mailbox (the owner's device-down policy, 2026-10-02; lifted from mqtt_ac's
//! `mailbox.rs`, mqtt_lutron's `hwi_mailbox.rs` and mqtt_ynca's connection mailbox).
//!
//! The MQTT session posts here and never waits; only the device worker takes, and only while the
//! device is connected. A bounded queue in its place parked the session once it was full while the
//! device was down — and the session is what reads the broker's CONNACK, so a broker blip during a
//! device outage left the bridge deaf (no-hang 3b, review B3).
//!
//! Each request is one of four kinds:
//! - **State** (a property's requested value), keyed by (target, property). A post replaces a
//!   waiting value with the same key and moves it to the back, so the device receives the latest
//!   value per key, in the order the values were last set. Held while the device is down, until
//!   the request's deadline (its MQTT message expiry): a value past it is dropped, never applied.
//!   The worker takes a target's waiting values together, for one atomic `apply`.
//! - **Config** (a target's setup), the latest per target; held while the device is down. A
//!   target's setup comes before its values: every held config is taken before any held value,
//!   and while up a waiting config of a target is taken before that target's values (review M4).
//!   Its `feedback` override takes effect once `configure` has taken it.
//! - **Momentary** (a command that acts once): queued in order while the device is connected, at
//!   most [`MOMENTARY_CAP`] (past it, refused); refused at once while it is down — by the time it
//!   is back the command would mean nothing — and refused if the device drops before taking it.
//! - **Read** (`refresh`, or a read-class command): coalesced per key and moved to the back;
//!   dropped while the device is down — its return reads everything anyway.
//!
//! **Held state and its conflict check.** A value posted while the device is down, a value the
//! broker replayed as retained (made while the bridge was not listening), and a value held again
//! are *checked*: before it is applied the worker reads the device and the outage conflict rule
//! decides it (Store kit spec §4.5). Each value says why ([`Check`]): the rule's "before" values
//! depend on it, and a live value taken together with a checked one is not checked for its
//! company (review MINOR 3). Held values are taken first when the device is back, one target at
//! a time ([`Mailbox::take_held`]), and stay in the mailbox until then — so a value set meanwhile
//! replaces its held one, which is never applied (mqtt_ac re-review C3).
//!
//! **Unproven writes** (`Writes::Unacknowledged`): an applied value is kept until a later
//! successful probe or read proves the device took it ([`Mailbox::proven`]) — by order, never by
//! time: the worker calls the driver one operation at a time, so any read that succeeds after the
//! `apply` returned was answered after the device had the value (mqtt_ynca, 3b round 3). A value
//! written into a link that dies before that is held again at the disconnect, unless a newer value
//! waits (mqtt_lutron, F5).
//!
//! **Target-level writes.** The Store publishes `Desired/{instance}/{target}` first, then each
//! property topic retained (Store kit spec §4.2); the property copies are skipped here — whether
//! the target write still waits or was already applied — so the device sees the values once,
//! together.
//!
//! Nothing waits under the lock and nothing is logged under it.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lock_order::sync::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::{debug, info};

use crate::driver::{CommandCall, Feedback};
use crate::replies::ReplyTo;
use crate::wire::Conflict;

/// Momentary commands that may wait for a connected device; past it, one is refused.
pub const MOMENTARY_CAP: usize = 64;

/// How long the property copies of a target-level write are expected after it: a copy arriving
/// later is a request of its own.
pub const COPY_WINDOW: Duration = Duration::from_secs(60);

/// A property's requested value.
#[derive(Debug, Clone, PartialEq)]
pub struct StateValue {
    pub target: String,
    pub property: String,
    pub value: Value,
    /// When it is dropped if not yet applied.
    pub deadline: Option<Instant>,
    pub conflict: Conflict,
    /// Why the outage conflict rule decides it, if it does; the mailbox sets it when the value is
    /// posted, and when it is held again.
    pub check: Check,
}

/// Why a value is checked by the outage conflict rule — which decides its "before" values (Store
/// kit spec §4.5). Neither: a live request to a connected device, applied as it is.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Check {
    /// The broker replayed it as retained: made while the bridge was not listening. Its "before"
    /// values are what the bridge knew when it stopped listening.
    pub replayed: bool,
    /// Posted, or held again, while the device was down. Its "before" values are what the bridge
    /// knew of the device when its outage began.
    pub held: bool,
}

impl Check {
    /// Whether the conflict rule decides the value at all.
    pub fn any(self) -> bool {
        self.replayed || self.held
    }
}

impl StateValue {
    fn same_key(&self, other: &StateValue) -> bool {
        self.target == other.target && self.property == other.property
    }

    fn is_key(&self, target: &str, property: &str) -> bool {
        self.target == target && self.property == property
    }

    fn expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| now >= d)
    }
}

/// What a read coalesces by.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReadKey {
    /// `refresh`: one target, or (`None`) all.
    Refresh(Option<String>),
    /// A read-class command.
    Command { target: Option<String>, command: String },
}

#[derive(Debug)]
enum Entry {
    State { value: StateValue, held: bool },
    Config { target: String, config: Option<Value>, held: bool },
    Momentary { target: Option<String>, call: CommandCall, reply: Option<ReplyTo>, deadline: Option<Instant> },
    Read { key: ReadKey, call: Option<CommandCall>, replies: Vec<ReplyTo> },
}

impl Entry {
    fn target(&self) -> Option<&str> {
        match self {
            Entry::State { value, .. } => Some(&value.target),
            Entry::Config { target, .. } => Some(target),
            Entry::Momentary { target, .. } => target.as_deref(),
            Entry::Read { key: ReadKey::Refresh(target), .. } | Entry::Read { key: ReadKey::Command { target, .. }, .. } => {
                target.as_deref()
            }
        }
    }
}

/// What the worker takes.
#[derive(Debug, Clone, PartialEq)]
pub enum Work {
    /// One target's waiting values, for one `apply`; those whose [`Check`] says so are decided by
    /// the conflict rule first, each on its own.
    Apply {
        target: String,
        values: Vec<StateValue>,
    },
    Configure {
        target: String,
        config: Option<Value>,
    },
    Execute {
        target: Option<String>,
        call: CommandCall,
        reply: Option<ReplyTo>,
    },
    Read {
        key: ReadKey,
        call: Option<CommandCall>,
        replies: Vec<ReplyTo>,
    },
}

/// How the outage conflict rule decides a checked value (Store kit spec §4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    Apply,
    /// The device already has it: nothing to write.
    Satisfied,
    /// A change at the device won.
    Superseded,
    /// The device's current value is unknown, and the change at the device wins
    /// (`DeviceWins`): not applied on a guess — reported superseded, saying why (review M6).
    Unknown,
}

/// A held target's values, in flight until [`Mailbox::applied`] or [`Mailbox::failed`]; the
/// worker decides them.
#[derive(Debug, Clone, PartialEq)]
pub struct HeldValues {
    pub target: String,
    pub values: Vec<StateValue>,
}

/// What the worker takes during the catch-up.
#[derive(Debug, Clone, PartialEq)]
pub enum Held {
    Values(HeldValues),
    Config { target: String, config: Option<Value> },
}

/// What became of a post.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Posted {
    /// Waiting for the connected device.
    Queued,
    /// A state value or config while the device is down: held for its return.
    Held,
    /// A read while the device is down.
    Dropped,
    /// A property copy of a target-level write, or a value already waiting.
    Skipped,
    /// A momentary command that will not be executed. The poster reports it on `Error`.
    Refused(Refusal),
}

/// Why a momentary command was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    DeviceDown,
    QueueFull,
}

impl Refusal {
    pub fn as_str(self) -> &'static str {
        match self {
            Refusal::DeviceDown => "the device is not connected",
            Refusal::QueueFull => "64 momentary commands are already waiting for the device",
        }
    }
}

/// A request the disconnect refused: a momentary command waiting, or a read whose answer someone
/// waits for.
#[derive(Debug, Clone, PartialEq)]
pub struct Unanswered {
    pub target: Option<String>,
    pub command: String,
    /// `true` for a momentary command (reported on `Error`); a read is only answered.
    pub momentary: bool,
    pub replies: Vec<ReplyTo>,
}

#[derive(Debug, Default)]
struct Inner {
    connected: bool,
    pending: VecDeque<Entry>,
    momentary: usize,
    /// State values the worker took and has not finished.
    in_flight: Vec<StateValue>,
    in_flight_config: Option<(String, Option<Value>)>,
    /// Values applied to a `Writes::Unacknowledged` device and not proven yet, in the order last
    /// written, each with when its key's oldest unproven write was made: a newer value of a key
    /// replaces its older one, and keeps the older one's proof deadline.
    unproven: Vec<(StateValue, Instant)>,
    /// The property copies a target-level write expects: the value, and until when.
    copies: HashMap<(String, String), (Value, Instant)>,
    /// Targets whose `Config` overrides the driver's feedback mode.
    feedback: HashMap<String, Feedback>,
    /// Momentary commands refused during this outage, for its recovery line.
    refused: u32,
    refusal_logged: bool,
    full_logged: bool,
    /// Held values dropped at their deadline, for the recovery line.
    expired: u32,
}

impl Inner {
    /// Replace a waiting value with the same key, and put this one at the back.
    fn replace_state(&mut self, value: StateValue, held: bool) {
        self.pending.retain(|e| !matches!(e, Entry::State { value: v, .. } if v.same_key(&value)));
        self.pending.push_back(Entry::State { value, held });
    }

    fn waits_for(&self, value: &StateValue) -> bool {
        self.pending.iter().any(|e| matches!(e, Entry::State { value: v, .. } if v.same_key(value)))
    }

    /// Note a refusal; `true` when it is the first of its episode — the outage, or the full queue.
    fn refuse(&mut self, why: Refusal) -> bool {
        match why {
            Refusal::DeviceDown => {
                self.refused += 1;
                !std::mem::replace(&mut self.refusal_logged, true)
            }
            Refusal::QueueFull => !std::mem::replace(&mut self.full_logged, true),
        }
    }

    /// Remove and return the group of State entries for the target of the entry at `first`: that
    /// one, and every later one for the same target with the same `held`, up to the first entry for
    /// that target that is not a state value (a command or a config keeps its place after them).
    fn take_group(&mut self, first: usize) -> (String, Vec<StateValue>) {
        let (target, held) = match &self.pending[first] {
            Entry::State { value, held, .. } => (value.target.clone(), *held),
            _ => unreachable!("take_group starts at a state value"),
        };
        let mut indices = Vec::new();
        for (i, entry) in self.pending.iter().enumerate().skip(first) {
            match entry {
                Entry::State { value, held: h, .. } if value.target == target && *h == held => indices.push(i),
                other if other.target() == Some(target.as_str()) => break,
                _ => {}
            }
        }
        let mut values = Vec::new();
        for i in indices.into_iter().rev() {
            if let Some(Entry::State { value, .. }) = self.pending.remove(i) {
                values.push(value);
            }
        }
        values.reverse();
        (target, values)
    }

    /// Drop the values past their deadline, counting them.
    fn drop_expired(&mut self, values: &mut Vec<StateValue>, now: Instant) -> usize {
        let before = values.len();
        values.retain(|v| !v.expired(now));
        let dropped = before - values.len();
        self.expired += dropped as u32;
        dropped
    }
}

/// The first refusal of an episode is an INFO, the rest DEBUG; the recovery INFO counts them.
/// Called with the mailbox's lock released.
fn log_refusal(command: &str, target: Option<&str>, why: Refusal, first: bool) {
    if first {
        info!(kind = "command_refused", command, target, reason = why.as_str(), "Command refused");
    } else {
        debug!(kind = "command_refused", command, target, reason = why.as_str(), "Command refused");
    }
}

/// See the module documentation.
#[derive(Debug)]
pub struct Mailbox {
    inner: Mutex<Inner>,
    posted: Notify,
}

impl Mailbox {
    /// A mailbox for a device not connected yet: until the worker's first connect, state is held
    /// for it, momentary commands refused and reads dropped, as in any outage (mqtt_ac re-review
    /// C10: started as connected, a momentary command posted before the first connect was queued
    /// and applied whenever that connect came).
    pub fn new() -> Arc<Mailbox> {
        Arc::new(Mailbox { inner: Mutex::new("mailbox-lock", Inner::default()), posted: Notify::new() })
    }

    fn locked(&self) -> lock_order::sync::MutexGuard<'_, Inner> {
        // WAIT: mailbox-lock
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn queued(&self, posted: Posted) -> Posted {
        if posted == Posted::Queued {
            self.posted.notify_one();
        }
        posted
    }

    /// A property's requested value; `replay`: the broker delivered it as retained, so it was made
    /// while the bridge was not listening, and is checked like a held one. Never waits.
    pub fn post_state(&self, mut value: StateValue, replay: bool) -> Posted {
        let now = Instant::now();
        let posted = {
            let mut inner = self.locked();
            let key = (value.target.clone(), value.property.clone());
            match inner.copies.remove(&key) {
                Some((copy, until)) if copy == value.value && now < until => Posted::Skipped,
                _ => {
                    let held = !inner.connected;
                    value.check = Check { replayed: replay, held };
                    inner.replace_state(value, held);
                    if held {
                        Posted::Held
                    } else {
                        Posted::Queued
                    }
                }
            }
        };
        self.queued(posted)
    }

    /// A target-level write: its values, applied together; their property copies, expected next,
    /// are skipped. Never waits.
    pub fn post_target(&self, values: Vec<StateValue>) -> Posted {
        let until = Instant::now() + COPY_WINDOW;
        let posted = {
            let mut inner = self.locked();
            let held = !inner.connected;
            for mut value in values {
                inner.copies.insert((value.target.clone(), value.property.clone()), (value.value.clone(), until));
                value.check = Check { replayed: false, held };
                inner.replace_state(value, held);
            }
            if held {
                Posted::Held
            } else {
                Posted::Queued
            }
        };
        self.queued(posted)
    }

    /// A property's request was retracted: nothing of it is applied or held again any more.
    pub fn retract(&self, target: &str, property: &str) {
        let mut inner = self.locked();
        inner.pending.retain(|e| !matches!(e, Entry::State { value, .. } if value.is_key(target, property)));
        inner.in_flight.retain(|v| !v.is_key(target, property));
        inner.unproven.retain(|(v, _)| !v.is_key(target, property));
        inner.copies.remove(&(target.to_owned(), property.to_owned()));
    }

    /// A target's setup, or (`None`) its retraction; the latest per target. Its `feedback`, if it
    /// names one, overrides the driver's for that target once `configure` has taken it
    /// ([`configured`](Self::configured)). Never waits.
    pub fn post_config(&self, target: &str, config: Option<Value>) -> Posted {
        let posted = {
            let mut inner = self.locked();
            inner.pending.retain(|e| !matches!(e, Entry::Config { target: t, .. } if t == target));
            let held = !inner.connected;
            inner.pending.push_back(Entry::Config { target: target.to_owned(), config, held });
            if held {
                Posted::Held
            } else {
                Posted::Queued
            }
        };
        self.queued(posted)
    }

    /// A momentary command. Never waits.
    pub fn post_command(&self, target: Option<String>, call: CommandCall, reply: Option<ReplyTo>, deadline: Option<Instant>) -> Posted {
        let (posted, refusal) = {
            let mut inner = self.locked();
            let why = if !inner.connected {
                Some(Refusal::DeviceDown)
            } else if inner.momentary >= MOMENTARY_CAP {
                Some(Refusal::QueueFull)
            } else {
                None
            };
            match why {
                None => {
                    inner.momentary += 1;
                    inner.pending.push_back(Entry::Momentary { target: target.clone(), call: call.clone(), reply, deadline });
                    (Posted::Queued, None)
                }
                Some(why) => (Posted::Refused(why), Some((why, inner.refuse(why)))),
            }
        };
        if let Some((why, first)) = refusal {
            log_refusal(&call.command, target.as_deref(), why, first);
        }
        self.queued(posted)
    }

    /// A read; coalesced with one waiting for the same key (the latest parameters, every reply).
    /// Never waits.
    pub fn post_read(&self, key: ReadKey, call: Option<CommandCall>, reply: Option<ReplyTo>) -> Posted {
        let posted = {
            let mut inner = self.locked();
            if !inner.connected {
                Posted::Dropped
            } else {
                let mut replies = Vec::new();
                if let Some(i) = inner.pending.iter().position(|e| matches!(e, Entry::Read { key: k, .. } if *k == key)) {
                    if let Some(Entry::Read { replies: earlier, .. }) = inner.pending.remove(i) {
                        replies = earlier;
                    }
                }
                replies.extend(reply);
                inner.pending.push_back(Entry::Read { key, call, replies });
                Posted::Queued
            }
        };
        self.queued(posted)
    }

    /// The device is connected (proven alive): posts are queued for it again, behind the state
    /// held while it was down, which the worker takes first. Returns how many momentary commands
    /// were refused while it was down.
    pub fn connected(&self) -> u32 {
        let refused = {
            let mut inner = self.locked();
            inner.connected = true;
            inner.refusal_logged = false;
            std::mem::take(&mut inner.refused)
        };
        self.posted.notify_one();
        refused
    }

    /// The device link is gone. What the device may not have taken is held again, ahead of what
    /// waits and in the order it was written — every unproven value, and what the worker had taken
    /// and not finished — unless a newer value for its key waits. Waiting state and configs stay,
    /// held; waiting reads are dropped; waiting momentary commands are refused. Returns the refused
    /// commands, and the dropped reads someone waits on an answer for, to be reported.
    pub fn disconnected(&self) -> Vec<Unanswered> {
        let mut unanswered = Vec::new();
        let first = {
            let mut inner = self.locked();
            let was_connected = std::mem::replace(&mut inner.connected, false);
            if was_connected {
                inner.refused = 0;
                inner.refusal_logged = false;
            }
            // Written first, then taken: unproven, then in flight; the newest value of a key wins.
            let mut again: Vec<StateValue> = inner.unproven.drain(..).map(|(v, _)| v).collect();
            again.append(&mut inner.in_flight);
            let mut newest: Vec<StateValue> = Vec::new();
            for value in again.into_iter().rev() {
                if !newest.iter().any(|v| v.same_key(&value)) && !inner.waits_for(&value) {
                    newest.push(value);
                }
            }
            // `newest` is newest first: each to the front leaves them oldest first.
            for mut value in newest {
                value.check.held = true;
                inner.pending.push_front(Entry::State { value, held: true });
            }
            // A config in flight goes first of all: a target's setup comes before its values.
            if let Some((target, config)) = inner.in_flight_config.take() {
                if !inner.pending.iter().any(|e| matches!(e, Entry::Config { target: t, .. } if *t == target)) {
                    inner.pending.push_front(Entry::Config { target, config, held: true });
                }
            }
            let waiting = std::mem::take(&mut inner.pending);
            for entry in waiting {
                match entry {
                    Entry::State { mut value, .. } => {
                        value.check.held = true;
                        inner.pending.push_back(Entry::State { value, held: true });
                    }
                    Entry::Config { target, config, .. } => inner.pending.push_back(Entry::Config { target, config, held: true }),
                    Entry::Momentary { target, call, reply, .. } => {
                        unanswered.push(Unanswered { target, command: call.command, momentary: true, replies: reply.into_iter().collect() })
                    }
                    Entry::Read { key, replies, .. } if !replies.is_empty() => {
                        let (target, command) = match key {
                            ReadKey::Refresh(target) => (target, "refresh".to_owned()),
                            ReadKey::Command { target, command } => (target, command),
                        };
                        unanswered.push(Unanswered { target, command, momentary: false, replies });
                    }
                    Entry::Read { .. } => {}
                }
            }
            inner.momentary = 0;
            inner.full_logged = false;
            let mut first = false;
            for _ in unanswered.iter().filter(|u| u.momentary) {
                first |= inner.refuse(Refusal::DeviceDown);
            }
            first
        };
        for (i, u) in unanswered.iter().filter(|u| u.momentary).enumerate() {
            log_refusal(&u.command, u.target.as_deref(), Refusal::DeviceDown, first && i == 0);
        }
        unanswered
    }

    /// The next of what was held while the device was down — a target's values, or a config — or
    /// `None` when nothing held is left. The values are in flight until [`applied`](Self::applied)
    /// or [`failed`](Self::failed); the worker decides them by the conflict rule first, and keeps
    /// in flight only what it applies ([`retain_in_flight`](Self::retain_in_flight)). Never waits.
    pub fn take_held(&self) -> Option<Held> {
        let now = Instant::now();
        let (held, expired) = {
            let mut inner = self.locked();
            let mut expired = 0;
            let held = loop {
                // Every held config before any held value: a target's setup comes first.
                if let Some(i) = inner.pending.iter().position(|e| matches!(e, Entry::Config { held: true, .. })) {
                    if let Some(Entry::Config { target, config, .. }) = inner.pending.remove(i) {
                        inner.in_flight_config = Some((target.clone(), config.clone()));
                        break Some(Held::Config { target, config });
                    }
                }
                match inner.pending.front() {
                    Some(Entry::State { held: true, .. }) => {}
                    _ => break None,
                }
                let (target, mut values) = inner.take_group(0);
                expired += inner.drop_expired(&mut values, now);
                if values.is_empty() {
                    continue;
                }
                inner.in_flight = values.clone();
                break Some(Held::Values(HeldValues { target, values }));
            };
            (held, expired)
        };
        if expired > 0 {
            debug!(expired, "Held requests dropped at their deadline");
        }
        held
    }

    /// The next work; what it dropped at its deadline on the way is counted in `dropped`, for the
    /// caller to log once the lock is released.
    fn next(inner: &mut Inner, now: Instant, dropped: &mut usize) -> Option<Work> {
        loop {
            match inner.pending.front()? {
                Entry::State { value, .. } => {
                    // A target's waiting config goes before its values.
                    let target = value.target.clone();
                    if let Some(i) = inner.pending.iter().position(|e| matches!(e, Entry::Config { target: t, .. } if *t == target)) {
                        if let Some(Entry::Config { target, config, .. }) = inner.pending.remove(i) {
                            inner.in_flight_config = Some((target.clone(), config.clone()));
                            return Some(Work::Configure { target, config });
                        }
                    }
                    let (target, mut values) = inner.take_group(0);
                    *dropped += inner.drop_expired(&mut values, now);
                    if values.is_empty() {
                        continue;
                    }
                    inner.in_flight = values.clone();
                    return Some(Work::Apply { target, values });
                }
                Entry::Config { .. } => {
                    if let Some(Entry::Config { target, config, .. }) = inner.pending.pop_front() {
                        inner.in_flight_config = Some((target.clone(), config.clone()));
                        return Some(Work::Configure { target, config });
                    }
                }
                Entry::Momentary { .. } => {
                    if let Some(Entry::Momentary { target, call, reply, deadline }) = inner.pending.pop_front() {
                        inner.momentary = inner.momentary.saturating_sub(1);
                        if inner.momentary == 0 {
                            inner.full_logged = false;
                        }
                        if deadline.is_some_and(|d| now >= d) {
                            *dropped += 1;
                            continue;
                        }
                        return Some(Work::Execute { target, call, reply });
                    }
                }
                Entry::Read { .. } => {
                    if let Some(Entry::Read { key, call, replies }) = inner.pending.pop_front() {
                        return Some(Work::Read { key, call, replies });
                    }
                }
            }
        }
    }

    /// The next work, if any waits. Never waits.
    pub fn try_take(&self) -> Option<Work> {
        let mut dropped = 0;
        let work = {
            let mut inner = self.locked();
            Mailbox::next(&mut inner, Instant::now(), &mut dropped)
        };
        if dropped > 0 {
            debug!(dropped, "Requests dropped at their deadline");
        }
        work
    }

    /// The next work, waiting for a post if there is none. The worker calls it only while the
    /// device is connected. Cancellation-safe: nothing is taken before the only await.
    pub async fn take(&self) -> Work {
        loop {
            if let Some(work) = self.try_take() {
                return work;
            }
            // WAIT: mailbox-posted
            self.posted.notified().await;
        }
    }

    /// The values the worker took, still as they were taken: each one still in flight — not
    /// retracted meanwhile — with no newer value of its key waiting, and before its deadline. The
    /// worker asks after a wait between taking values and applying them (a conflict read); what is
    /// dropped leaves the in-flight set, and one past its deadline is counted (review C8: the
    /// worker applied its private copy of a request retracted meanwhile). Never waits.
    pub fn revalidate(&self, values: Vec<StateValue>) -> Vec<StateValue> {
        let now = Instant::now();
        let (kept, dropped) = {
            let mut inner = self.locked();
            let mut kept = Vec::new();
            let mut dropped = 0;
            for value in values {
                let in_flight = inner.in_flight.contains(&value);
                if in_flight && !inner.waits_for(&value) && !value.expired(now) {
                    kept.push(value);
                } else {
                    if in_flight && value.expired(now) {
                        inner.expired += 1;
                    }
                    dropped += 1;
                }
            }
            inner.in_flight.retain(|v| kept.contains(v));
            (kept, dropped)
        };
        if dropped > 0 {
            debug!(dropped, "Requests retracted, replaced or expired while the device was read; not applied");
        }
        kept
    }

    /// Keep in flight only the values `keep` says (the rest were decided by the conflict rule).
    pub fn retain_in_flight(&self, keep: impl Fn(&StateValue) -> bool) {
        self.locked().in_flight.retain(|v| keep(v));
    }

    /// The values in flight were applied. To a `Writes::Unacknowledged` device they stay unproven
    /// until [`proven`](Self::proven), each replacing an older unproven value of its key — and
    /// keeping that one's time: the oldest outstanding proof deadline stands, so a stream of writes
    /// to one property cannot postpone the busy probe for good (review C5).
    pub fn applied(&self, unacknowledged: bool) {
        let now = Instant::now();
        let mut inner = self.locked();
        let applied = std::mem::take(&mut inner.in_flight);
        inner.in_flight_config = None;
        if unacknowledged {
            for value in applied {
                let at = match inner.unproven.iter().position(|(v, _)| v.same_key(&value)) {
                    Some(i) => inner.unproven.remove(i).1,
                    None => now,
                };
                inner.unproven.push((value, at));
            }
        }
    }

    /// `configure` took the config in flight for `target`: its `feedback`, if it names one, now
    /// overrides the driver's for that target, and a config naming none (or a retraction) ends an
    /// override (review, mailbox.rs:394: the override took effect at the post, and outlived a
    /// refusal).
    pub fn configured(&self, target: &str, config: Option<&Value>) {
        let mut inner = self.locked();
        inner.in_flight_config = None;
        match config.and_then(|c| c.get("feedback")).and_then(Value::as_str).and_then(Feedback::parse) {
            Some(feedback) => inner.feedback.insert(target.to_owned(), feedback),
            None => inner.feedback.remove(target),
        };
    }

    /// What was in flight failed on its own (refused, unusable, unsupported): passed over.
    pub fn failed(&self) {
        let mut inner = self.locked();
        inner.in_flight.clear();
        inner.in_flight_config = None;
    }

    /// A probe or read succeeded after every unproven write: they are proven.
    pub fn proven(&self) {
        self.locked().unproven.clear();
    }

    /// When the oldest unproven write was made, if one is unproven.
    pub fn unproven_since(&self) -> Option<Instant> {
        self.locked().unproven.iter().map(|(_, at)| *at).min()
    }

    /// Whether any work waits.
    pub fn waiting(&self) -> bool {
        !self.locked().pending.is_empty()
    }

    /// A target's feedback mode: its `Config`'s, else `default`.
    pub fn feedback(&self, target: &str, default: Feedback) -> Feedback {
        self.locked().feedback.get(target).copied().unwrap_or(default)
    }

    /// Held values dropped at their deadline since the last call.
    pub fn take_expired(&self) -> u32 {
        std::mem::take(&mut self.locked().expired)
    }
}

#[cfg(test)]
mod tests;

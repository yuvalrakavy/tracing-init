//! The mailbox's rules, one at a time (the owner's device-down policy; Store kit spec §4).

use std::time::{Duration, Instant};

use serde_json::json;

use super::*;
use crate::driver::PropertyMap;
use crate::test_support::Capture;

fn value(target: &str, property: &str, v: Value) -> StateValue {
    StateValue {
        target: target.into(),
        property: property.into(),
        value: v,
        deadline: None,
        conflict: Conflict::DeviceWins,
        check: Check::default(),
    }
}

fn call(command: &str) -> CommandCall {
    CommandCall { command: command.into(), params: PropertyMap::new() }
}

fn up() -> Arc<Mailbox> {
    let mailbox = Mailbox::new();
    mailbox.connected();
    mailbox
}

/// Everything `take` would hand the worker now, in order.
fn drain(mailbox: &Mailbox) -> Vec<Work> {
    let mut taken = Vec::new();
    while let Some(work) = mailbox.try_take() {
        mailbox.applied(false);
        taken.push(work);
    }
    taken
}

/// The (target, property, value) of each value in `work`'s applies.
fn applied(work: &[Work]) -> Vec<(String, String, Value)> {
    work.iter()
        .flat_map(|w| match w {
            Work::Apply { values, .. } => values.iter().map(|v| (v.target.clone(), v.property.clone(), v.value.clone())).collect(),
            _ => Vec::new(),
        })
        .collect()
}

fn held_values(mailbox: &Mailbox) -> Vec<HeldValues> {
    let mut held = Vec::new();
    while let Some(h) = mailbox.take_held() {
        if let Held::Values(v) = h {
            mailbox.applied(false);
            held.push(v);
        }
    }
    held
}

/// A new mailbox counts the device down until the first connect: state is held, a momentary
/// command refused, a read dropped (mqtt_ac re-review C10).
#[test]
fn a_new_mailbox_is_down_until_the_first_connect() {
    let mailbox = Mailbox::new();
    assert_eq!(mailbox.post_state(value("u1", "power", json!(true)), false), Posted::Held);
    assert_eq!(mailbox.post_command(Some("u1".into()), call("reset"), None, None), Posted::Refused(Refusal::DeviceDown));
    assert_eq!(mailbox.post_read(ReadKey::Refresh(None), None, None), Posted::Dropped);
    assert!(mailbox.try_take().is_some(), "the held value is in the mailbox");
}

/// Held state keeps the latest value per (target, property), in the order the values were last
/// set; the catch-up takes it a target at a time, its values together.
#[test]
fn held_state_keeps_the_latest_value_per_key_in_the_order_last_set() {
    let mailbox = Mailbox::new();
    mailbox.post_state(value("u1", "power", json!(true)), false);
    mailbox.post_state(value("u2", "temp", json!(20)), false);
    mailbox.post_state(value("u1", "temp", json!(21)), false);
    mailbox.post_state(value("u1", "power", json!(false)), false);
    mailbox.connected();
    let held = held_values(&mailbox);
    let groups: Vec<(String, Vec<(String, Value)>)> =
        held.iter().map(|h| (h.target.clone(), h.values.iter().map(|v| (v.property.clone(), v.value.clone())).collect())).collect();
    assert_eq!(
        groups,
        vec![
            ("u2".to_owned(), vec![("temp".to_owned(), json!(20))]),
            ("u1".to_owned(), vec![("temp".to_owned(), json!(21)), ("power".to_owned(), json!(false))])
        ]
    );
}

/// A value posted after the device is back replaces its held one, which is never applied (mqtt_ac
/// re-review C3).
#[test]
fn a_value_posted_after_the_return_replaces_its_held_one() {
    let mailbox = Mailbox::new();
    mailbox.post_state(value("u1", "temp", json!(20)), false);
    mailbox.connected();
    mailbox.post_state(value("u1", "temp", json!(22)), false);
    assert!(held_values(&mailbox).is_empty(), "a held value superseded after the return was still taken as held");
    assert_eq!(applied(&drain(&mailbox)), vec![("u1".into(), "temp".into(), json!(22))]);
}

/// A held value past its deadline (its request's MQTT message expiry) is dropped, never applied,
/// and counted for the recovery line.
#[test]
fn a_held_value_past_its_deadline_is_dropped() {
    let mailbox = Mailbox::new();
    let mut late = value("u1", "temp", json!(22));
    late.deadline = Some(Instant::now() - Duration::from_millis(1));
    let mut timely = value("u1", "power", json!(true));
    timely.deadline = Some(Instant::now() + Duration::from_secs(60));
    mailbox.post_state(late, false);
    mailbox.post_state(timely, false);
    mailbox.connected();
    let held = held_values(&mailbox);
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].values.iter().map(|v| v.property.as_str()).collect::<Vec<_>>(), ["power"], "a value past its deadline was applied");
    assert_eq!(mailbox.take_expired(), 1, "the expired value was not counted");
}

/// While up, a target's waiting values are taken together for one `apply` — but not past a
/// command for that target, which keeps its place after them.
#[test]
fn a_targets_values_are_taken_together_up_to_a_command_for_it() {
    let mailbox = up();
    mailbox.post_target(vec![value("tv", "power", json!(true)), value("tv", "input", json!("hdmi1"))]);
    mailbox.post_state(value("amp", "volume", json!(30)), false);
    mailbox.post_command(Some("tv".into()), call("learn"), None, None);
    mailbox.post_state(value("tv", "volume", json!(10)), false);
    let work = drain(&mailbox);
    let shape: Vec<String> = work
        .iter()
        .map(|w| match w {
            Work::Apply { target, values, .. } => {
                format!("{target}:{}", values.iter().map(|v| v.property.as_str()).collect::<Vec<_>>().join("+"))
            }
            Work::Execute { call, .. } => call.command.clone(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(shape, ["tv:power+input", "amp:volume", "learn", "tv:volume"]);
}

/// The property copies of a target-level write are skipped — while it waits and after it was
/// applied — so the device sees the values once; a copy with another value is a request of its own.
#[test]
fn the_property_copies_of_a_target_write_are_skipped() {
    let mailbox = up();
    mailbox.post_target(vec![value("tv", "power", json!(true)), value("tv", "input", json!("hdmi1"))]);
    assert_eq!(mailbox.post_state(value("tv", "power", json!(true)), false), Posted::Skipped);
    let first = drain(&mailbox);
    assert_eq!(first.len(), 1, "the target write was not one apply: {first:?}");
    assert_eq!(
        mailbox.post_state(value("tv", "input", json!("hdmi1")), false),
        Posted::Skipped,
        "a copy after the apply was applied again"
    );
    assert_eq!(mailbox.post_state(value("tv", "input", json!("hdmi2")), false), Posted::Queued);
    assert_eq!(applied(&drain(&mailbox)), vec![("tv".into(), "input".into(), json!("hdmi2"))]);
    assert_eq!(mailbox.post_state(value("tv", "power", json!(true)), false), Posted::Queued, "a copy is skipped once only");
}

/// A retraction removes the request wherever it is: waiting, held, in flight, unproven.
#[test]
fn a_retraction_removes_the_request_everywhere() {
    let mailbox = Mailbox::new();
    mailbox.post_state(value("u1", "temp", json!(22)), false);
    mailbox.retract("u1", "temp");
    mailbox.connected();
    assert!(held_values(&mailbox).is_empty(), "a retracted held value was applied");

    mailbox.post_state(value("u1", "power", json!(true)), false);
    assert!(mailbox.try_take().is_some());
    mailbox.retract("u1", "power");
    mailbox.applied(true);
    assert!(mailbox.unproven_since().is_none(), "a value retracted in flight became unproven");
    mailbox.disconnected();
    mailbox.connected();
    assert!(held_values(&mailbox).is_empty(), "a value retracted in flight was held again");
}

/// A momentary command is queued while the device is up, refused while it is down; past
/// `MOMENTARY_CAP` waiting, refused; those waiting when the device goes down are refused and
/// returned to be reported. Reads are coalesced while up, dropped while down.
#[test]
fn momentary_commands_and_reads() {
    let mailbox = up();
    for _ in 0..MOMENTARY_CAP {
        assert_eq!(mailbox.post_command(None, call("pulse"), None, None), Posted::Queued);
    }
    assert_eq!(
        mailbox.post_command(None, call("pulse"), None, None),
        Posted::Refused(Refusal::QueueFull),
        "a momentary command past the cap was queued"
    );
    let reply = ReplyTo { topic: "R/Event/i".into(), correlation: None };
    assert_eq!(mailbox.post_read(ReadKey::Refresh(Some("u1".into())), None, Some(reply.clone())), Posted::Queued);
    assert_eq!(mailbox.post_read(ReadKey::Refresh(Some("u1".into())), None, Some(reply.clone())), Posted::Queued);
    let unanswered = mailbox.disconnected();
    assert_eq!(unanswered.iter().filter(|u| u.momentary).count(), MOMENTARY_CAP, "the waiting momentary commands were not refused");
    let reads: Vec<&Unanswered> = unanswered.iter().filter(|u| !u.momentary).collect();
    assert_eq!(reads.len(), 1, "the reads were not coalesced");
    assert_eq!(reads[0].replies.len(), 2, "a coalesced read lost a reply");
    assert!(mailbox.try_take().is_none(), "a momentary command or read survived the disconnect");
    assert_eq!(mailbox.post_command(None, call("pulse"), None, None), Posted::Refused(Refusal::DeviceDown));
    assert_eq!(mailbox.connected(), MOMENTARY_CAP as u32 + 1, "the refusals of the outage were not counted");
}

/// A momentary command still waiting at its deadline (its message expiry) is dropped, not run.
#[test]
fn a_momentary_command_past_its_deadline_is_dropped() {
    let mailbox = up();
    mailbox.post_command(None, call("late"), None, Some(Instant::now() - Duration::from_millis(1)));
    mailbox.post_command(None, call("timely"), None, Some(Instant::now() + Duration::from_secs(60)));
    let commands: Vec<String> = drain(&mailbox)
        .into_iter()
        .filter_map(|w| match w {
            Work::Execute { call, .. } => Some(call.command),
            _ => None,
        })
        .collect();
    assert_eq!(commands, ["timely"], "a momentary command past its deadline was run");
}

/// A write to an unacknowledged device stays unproven until a later probe or read; if the link
/// dies first it is held again — ahead of what waits, in the order written — unless a newer value
/// for its key waits (mqtt_lutron, mqtt_ynca; the fleet's F5).
#[test]
fn unproven_writes_are_held_again_unless_a_newer_value_waits() {
    let mailbox = up();
    mailbox.post_state(value("a", "level", json!(10)), false);
    mailbox.post_state(value("b", "level", json!(20)), false);
    for _ in 0..2 {
        assert!(mailbox.try_take().is_some());
        mailbox.applied(true);
    }
    assert!(mailbox.unproven_since().is_some());
    mailbox.post_state(value("b", "level", json!(25)), false);
    mailbox.post_state(value("c", "level", json!(30)), false);
    mailbox.disconnected();
    mailbox.connected();
    let held = held_values(&mailbox);
    let order: Vec<(String, Value)> = held.iter().flat_map(|h| h.values.iter().map(|v| (v.target.clone(), v.value.clone()))).collect();
    assert_eq!(order, vec![("a".into(), json!(10)), ("b".into(), json!(25)), ("c".into(), json!(30))], "the held-again order is wrong");

    // Proven: nothing is held again.
    let mailbox = up();
    mailbox.post_state(value("a", "level", json!(10)), false);
    assert!(mailbox.try_take().is_some());
    mailbox.applied(true);
    mailbox.proven();
    mailbox.disconnected();
    mailbox.connected();
    assert!(held_values(&mailbox).is_empty(), "a proven write was held again");
}

/// Replacing an unproven value keeps the oldest outstanding proof deadline: a stream of writes to
/// one property, each replacing the last before it was proven, must not postpone the busy probe
/// for good (review C5).
#[test]
fn replacing_an_unproven_value_keeps_the_oldest_proof_deadline() {
    let mailbox = up();
    mailbox.post_state(value("a", "level", json!(1)), false);
    assert!(mailbox.try_take().is_some());
    mailbox.applied(true);
    let first = mailbox.unproven_since().expect("the write is unproven");
    std::thread::sleep(Duration::from_millis(20));
    mailbox.post_state(value("a", "level", json!(2)), false);
    assert!(mailbox.try_take().is_some());
    mailbox.applied(true);
    assert_eq!(mailbox.unproven_since(), Some(first), "a newer write of the same key reset the proof deadline");
    mailbox.proven();
    assert_eq!(mailbox.unproven_since(), None);
}

/// A value the worker took and could not apply, the link lost, is held again for the next link.
#[test]
fn a_value_in_flight_when_the_link_dies_is_held_again() {
    let mailbox = up();
    mailbox.post_state(value("a", "level", json!(10)), false);
    assert!(mailbox.try_take().is_some());
    mailbox.disconnected();
    mailbox.connected();
    assert_eq!(held_values(&mailbox).len(), 1, "the value in flight was lost with the link");
}

/// A value the broker replayed as retained is checked by the conflict rule, like a held one —
/// each value on its own: a live value taken with a replayed one for the same target is not
/// (review MINOR 3).
#[test]
fn a_replayed_value_is_checked_on_its_own() {
    let mailbox = up();
    mailbox.post_state(value("a", "level", json!(10)), true);
    mailbox.post_state(value("a", "mode", json!("cool")), false);
    mailbox.post_state(value("b", "level", json!(20)), false);
    let checks: Vec<(String, Check)> = drain(&mailbox)
        .iter()
        .flat_map(|w| match w {
            Work::Apply { values, .. } => values.iter().map(|v| (v.property.clone(), v.check)).collect(),
            _ => Vec::new(),
        })
        .collect();
    let replayed = Check { replayed: true, held: false };
    assert_eq!(
        checks,
        [("level".to_owned(), replayed), ("mode".to_owned(), Check::default()), ("level".to_owned(), Check::default())],
        "the checks are not each value's own"
    );
    // Held while down, and held again at a disconnect: checked as held.
    let mailbox = Mailbox::new();
    mailbox.post_state(value("a", "level", json!(1)), false);
    mailbox.connected();
    let Some(Held::Values(held)) = mailbox.take_held() else { panic!("nothing held") };
    assert_eq!(held.values[0].check, Check { replayed: false, held: true });
}

/// A config is the latest per target, held while the device is down; one naming a feedback mode
/// overrides the driver's for that target at once.
#[test]
fn configs_are_the_latest_per_target_and_may_set_the_feedback() {
    let mailbox = Mailbox::new();
    mailbox.post_config("tv", Some(json!({"code": "a"})));
    mailbox.post_config("tv", Some(json!({"code": "b", "feedback": "assumed"})));
    assert_eq!(mailbox.feedback("tv", Feedback::Observed), Feedback::Assumed);
    mailbox.connected();
    let mut configs = Vec::new();
    while let Some(held) = mailbox.take_held() {
        mailbox.applied(false);
        configs.push(held);
    }
    assert_eq!(configs, vec![Held::Config { target: "tv".into(), config: Some(json!({"code": "b", "feedback": "assumed"})) }]);
    mailbox.post_config("tv", None);
    assert_eq!(mailbox.feedback("tv", Feedback::Observed), Feedback::Observed, "a retracted config kept its feedback override");
}

/// A value retracted, replaced or past its deadline while the worker read the device is not
/// applied when the read returns; one still as it was is (review C8).
#[test]
fn a_value_retracted_replaced_or_expired_during_a_read_is_dropped() {
    let mailbox = up();
    let mut late = value("u1", "c", json!(3));
    late.deadline = Some(Instant::now() + Duration::from_millis(30));
    for v in [value("u1", "a", json!(1)), value("u1", "b", json!(2)), late, value("u1", "d", json!(4))] {
        mailbox.post_state(v, true);
    }
    let Some(Work::Apply { values, .. }) = mailbox.try_take() else { panic!("nothing taken") };
    // Meanwhile: a retracted, b replaced, c past its deadline.
    mailbox.retract("u1", "a");
    mailbox.post_state(value("u1", "b", json!(20)), false);
    std::thread::sleep(Duration::from_millis(40));
    let kept = mailbox.revalidate(values);
    assert_eq!(kept.iter().map(|v| v.property.as_str()).collect::<Vec<_>>(), ["d"], "a value changed during the read was kept");
    assert_eq!(mailbox.take_expired(), 1, "the value past its deadline was not counted");
}

/// Nothing is logged while the mailbox's lock is held.
#[test]
fn nothing_is_logged_under_the_mailbox_lock() {
    use std::cell::Cell;
    use std::rc::Rc;

    let mailbox = up();
    let (under_lock, events) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let _watch = {
        let (mailbox, under_lock, events) = (mailbox.clone(), under_lock.clone(), events.clone());
        crate::test_support::watch(move |_| {
            events.set(events.get() + 1);
            if mailbox.inner.try_lock().is_err() {
                under_lock.set(under_lock.get() + 1);
            }
        })
    };
    for _ in 0..MOMENTARY_CAP + 2 {
        mailbox.post_command(None, call("pulse"), None, None);
    }
    mailbox.disconnected();
    mailbox.post_command(None, call("pulse"), None, None);
    let mut late = value("u1", "temp", json!(1));
    late.deadline = Some(Instant::now() - Duration::from_millis(1));
    mailbox.connected();
    mailbox.post_state(late.clone(), false);
    let _ = mailbox.try_take();
    mailbox.disconnected();
    mailbox.post_state(late, false);
    mailbox.connected();
    let _ = mailbox.take_held();
    assert!(events.get() >= 5, "the mailbox logged nothing, so this test proves nothing");
    assert_eq!(under_lock.get(), 0, "the mailbox logged while holding its lock");
}

/// The first refusal of an outage is an INFO `command_refused`, the rest DEBUG.
#[test]
fn the_first_refusal_of_an_outage_is_info_and_the_rest_debug() {
    let log = Capture::start();
    let mailbox = Mailbox::new();
    for _ in 0..3 {
        mailbox.post_command(None, call("pulse"), None, None);
    }
    let refused = log.of_kind("command_refused");
    let levels: Vec<tracing::Level> = refused.iter().map(|r| r.level).collect();
    assert_eq!(levels, [tracing::Level::INFO, tracing::Level::DEBUG, tracing::Level::DEBUG]);
}

/// `take` waits for a post, and a post never waits.
#[tokio::test]
async fn take_waits_for_a_post_and_a_post_never_waits() {
    let mailbox = up();
    let taker = tokio::spawn({
        let mailbox = mailbox.clone();
        async move { mailbox.take().await }
    });
    tokio::task::yield_now().await;
    assert!(!taker.is_finished());
    mailbox.post_state(value("u1", "power", json!(true)), false);
    let work = tokio::time::timeout(Duration::from_secs(5), taker).await.expect("take did not wake").unwrap();
    assert!(matches!(work, Work::Apply { .. }));
}

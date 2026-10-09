//! The reports: a model that coalesces, capped event queues, nothing waiting and nothing logged
//! under the lock.

use serde_json::json;

use super::*;
use crate::test_support::Capture;
use crate::wire::Reason;

fn values(pairs: &[(&str, Value)]) -> PropertyMap {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

/// A target's reports merge into its state and coalesce between takes; one that changes nothing
/// is not published again; `assumed` is part of the state.
#[test]
fn states_merge_and_coalesce() {
    let reports = Reports::new();
    reports.state("u1", values(&[("power", json!(true))]), false);
    reports.state("u1", values(&[("temp", json!(21))]), false);
    let changes = reports.take();
    assert_eq!(changes.states.len(), 1, "two reports of one target were not coalesced");
    let document: Value = serde_json::from_slice(&changes.states[0].1).unwrap();
    assert_eq!(document, json!({"power": true, "temp": 21}));
    reports.state("u1", values(&[("temp", json!(21))]), false);
    assert!(reports.take().states.is_empty(), "a report that changed nothing was published again");
    reports.state("u1", values(&[]), true);
    let assumed: Value = serde_json::from_slice(&reports.take().states[0].1).unwrap();
    assert_eq!(assumed, json!({"power": true, "temp": 21, "assumed": true}));
}

/// After a CONNACK the whole model is published again, and nothing waits any more; queued
/// messages stay queued.
#[test]
fn the_model_is_everything_known() {
    let reports = Reports::new();
    reports.status(LinkStatus::Connected);
    reports.state("a", values(&[("x", json!(1))]), false);
    reports.state("b", values(&[("y", json!(2))]), false);
    reports.error(ErrorReport::new(Reason::Rejected, "no"));
    let model = reports.model();
    assert_eq!(model.status, Some(LinkStatus::Connected));
    assert_eq!(model.states.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["a", "b"]);
    let changes = reports.take();
    assert!(changes.status.is_none() && changes.states.is_empty(), "what the model published was still waiting");
    assert_eq!(changes.errors.len(), 1, "the model took a queued error");
}

/// Past their caps the oldest `Error` and `Event` messages are displaced: one WARN per episode
/// each, and an INFO with how many when the session takes the queue again.
#[test]
fn messages_past_the_cap_displace_the_oldest_said_once_per_episode() {
    let log = Capture::start();
    let reports = Reports::new();
    for i in 0..ERROR_CAP + 5 {
        reports.error(ErrorReport::new(Reason::Rejected, format!("{i}")));
        reports.event(Outbound { topic: None, payload: format!("{i}").into_bytes(), correlation: None });
    }
    assert_eq!(log.of_kind("error_report_dropped").len(), 1);
    assert_eq!(log.of_kind("event_report_dropped").len(), 1);
    let changes = reports.take();
    assert_eq!(changes.errors.len(), ERROR_CAP);
    assert_eq!(changes.errors[0].error, "5", "the oldest were not the ones displaced");
    assert_eq!(changes.events.len(), EVENT_CAP);
    let ended = log.of_kind("error_report_drop_ended");
    assert!(ended.len() == 1 && ended[0].field("dropped") == Some("5"), "{:#?}", log.records());
    assert_eq!(log.of_kind("event_report_drop_ended").len(), 1);
}

/// Nothing is logged while the reports' lock is held.
#[test]
fn nothing_is_logged_under_the_reports_lock() {
    use std::cell::Cell;
    use std::rc::Rc;

    let reports = Reports::new();
    let (under_lock, events) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(0)));
    let _watch = {
        let (reports, under_lock, events) = (reports.clone(), under_lock.clone(), events.clone());
        crate::test_support::watch(move |_| {
            events.set(events.get() + 1);
            if reports.state.try_lock().is_err() {
                under_lock.set(under_lock.get() + 1);
            }
        })
    };
    for i in 0..ERROR_CAP + 1 {
        reports.error(ErrorReport::new(Reason::Rejected, format!("{i}")));
    }
    let _ = reports.take();
    assert!(events.get() >= 2, "the reports logged nothing, so this test proves nothing");
    assert_eq!(under_lock.get(), 0, "the reports logged while holding their lock");
}

/// A report wakes the session, and a report made while nobody waits leaves a permit.
#[tokio::test]
async fn a_report_wakes_the_session_and_never_waits() {
    let reports = Reports::new();
    reports.status(LinkStatus::Connecting);
    tokio::time::timeout(std::time::Duration::from_secs(5), reports.reported())
        .await
        .expect("a report made before the wait left no permit");
    assert!(reports.untaken());
}

/// A target or property name that cannot stand as one topic segment is dropped where it is
/// reported, with one ERROR `driver_contract_violation` per name, however often it is reported
/// (review B2).
#[test]
fn a_name_that_is_no_topic_segment_is_dropped_where_it_is_reported() {
    let log = Capture::start();
    let reports = Reports::new();
    for _ in 0..3 {
        reports.state("a#b", values(&[("x", json!(1))]), false);
        reports.state("t2", values(&[("a/b", json!(1)), ("ok", json!(2))]), false);
    }
    let model = reports.model();
    assert!(model.states.iter().all(|(t, _)| t != "a#b"), "a target that is no topic segment reached the model: {model:?}");
    let t2: Value = serde_json::from_slice(&model.states.iter().find(|(t, _)| t == "t2").expect("t2").1).unwrap();
    assert_eq!(t2, json!({"ok": 2}), "a property that is no topic segment reached the model");
    assert_eq!(log.of_kind("driver_contract_violation").len(), 2, "not one ERROR per bad name: {:#?}", log.records());
}

/// What an acknowledged write left is what the bridge knows for the conflict rule's snapshots —
/// the device snapshot and the broker-loss one — and never published; a report of the key replaces
/// it, and a key the driver reported during the write keeps its report (the lead's F2).
#[test]
fn an_acknowledged_write_is_known_to_the_snapshots_and_never_published() {
    let reports = Reports::new();
    reports.state("u1", values(&[("temp", json!(20)), ("power", json!(false))]), false);
    let _ = reports.take();
    let before = reports.reports_for("u1");
    // During the write, the driver reports power itself.
    reports.state("u1", values(&[("power", json!(true))]), false);
    let _ = reports.take();
    reports.written("u1", &values(&[("temp", json!(23)), ("power", json!(false))]), before);
    reports.written("u2", &values(&[("level", json!(40))]), 0);
    assert!(reports.take().states.is_empty(), "a write was published as State");
    assert_eq!(reports.model().states.len(), 1, "a written-only target was published");
    let snapshot = reports.snapshot();
    assert_eq!(snapshot["u1"], values(&[("temp", json!(23)), ("power", json!(true))]), "the snapshot is not what the bridge knows");
    assert_eq!(snapshot["u2"], values(&[("level", json!(40))]));
    reports.broker_lost();
    assert_eq!(reports.broker_before().1["u1"]["temp"], json!(23), "the broker-loss snapshot missed the write");
    // A later report replaces what the write left.
    reports.state("u1", values(&[("temp", json!(22))]), false);
    assert_eq!(reports.snapshot()["u1"]["temp"], json!(22), "a report did not replace the written value");
    reports.remove("u2");
    assert!(!reports.snapshot().contains_key("u2"), "a removed target kept its written values");
}

/// What the runtime assumes from an unacknowledged write is published, but left out of what the
/// conflict rule reads — the current values and the snapshots — until a probe or read proves the
/// write; then it stands (the R1 re-gate's R3).
#[test]
fn an_unproven_assumption_is_left_out_until_proven() {
    let reports = Reports::new();
    reports.state("u1", values(&[("level", json!(0))]), true);
    reports.assume_unproven("u1", values(&[("level", json!(40))]));
    reports.assume_unproven("u1", values(&[("level", json!(50))]));
    let published: Value = serde_json::from_slice(&reports.take().states[0].1).unwrap();
    assert_eq!(published, json!({"level": 50, "assumed": true}), "the assumption was not published");
    assert_eq!(reports.values_of("u1").unwrap()["level"], json!(0), "an unproven assumption counted as the device's value");
    assert_eq!(reports.snapshot()["u1"]["level"], json!(0), "a snapshot counted an unproven assumption");
    let written = crate::mailbox::StateValue {
        target: "u1".into(),
        property: "level".into(),
        value: json!(50),
        deadline: None,
        conflict: crate::wire::Conflict::DeviceWins,
        check: Default::default(),
        basis: None,
        reheld: false,
    };
    reports.assumptions_proven(&[written]);
    assert_eq!(reports.values_of("u1").unwrap()["level"], json!(50), "a proven assumption was left out");
    assert_eq!(reports.snapshot()["u1"]["level"], json!(50));
}

/// The read-back the conflict rule reads is frozen when it is first done: a retained `State` that
/// arrives after an incomplete read-back goes to the read-back a restore reads, never to the
/// "before" values in mid-process (Claude's re-gate minor 11).
#[test]
fn a_late_state_does_not_change_the_read_back_basis() {
    let reports = Reports::new();
    reports.read_back("u1", values(&[("level", json!(10))]));
    reports.read_back_incomplete();
    reports.read_back("u1", values(&[("level", json!(40))]));
    assert_eq!(reports.read_back_before().1["u1"]["level"], json!(10), "a late State changed the conflict rule's read-back");
    assert_eq!(reports.broker_basis("u1", "level").before, Some(json!(10)));
    assert_eq!(reports.read_back_all()["u1"]["level"], json!(40), "a late State did not reach the read-back a restore reads");
    assert!(!reports.read_back_is_complete());
    reports.read_back_complete();
    assert!(reports.read_back_is_complete());
    assert_eq!(reports.read_back_before().1["u1"]["level"], json!(10), "completing it late changed the frozen read-back");
}

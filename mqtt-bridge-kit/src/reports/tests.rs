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

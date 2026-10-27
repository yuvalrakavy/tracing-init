//! The reports: a model that coalesces, capped event queues, nothing waiting and nothing logged
//! under the lock.

use serde_json::json;

use super::*;
use crate::test_support::Capture;
use crate::wire::Reason;

fn values(pairs: &[(&str, Value)]) -> PropertyMap {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect()
}

#[test]
fn exact_replacement_retires_observations_but_accepts_fresh_same_generation_updates() {
    let reports = Reports::new();
    let table = crate::TargetGenerations { instance: "instance.1".into(), targets: [("u1".into(), "generation.a".into())].into() };
    reports.install_policy(RequestPolicy::Fenced(table.clone()));
    let generation = RequestContext::new("generation.a").unwrap();
    let old = reports.observation(&generation);
    let image = crate::RecoveryImage {
        snapshot: None,
        context: RequestContext::default(),
        generations: table,
        states: [("u1".into(), values(&[("temp", json!(99))]))].into(),
        configurations: BTreeMap::new(),
    };
    reports.replace_recovery(&image).unwrap();
    let other_owner = Reports::new().observation(&generation);
    for stale in [old, other_owner] {
        reports.state_with_context("u1", values(&[("temp", json!(22))]), false, &stale);
        reports.read_back_with_context("u1", values(&[("temp", json!(22))]), &stale);
        reports.written_with_context("u1", &values(&[("temp", json!(22))]), 0, &stale);
        assert_eq!(reports.values_of("u1").unwrap()["temp"], json!(99), "retired observation changed exact model");
        assert_eq!(reports.snapshot()["u1"]["temp"], json!(99), "retired observation changed exact conflict cache");
        reports.remove_with_context("u1", &stale);
        assert!(reports.values_of("u1").is_some(), "retired observation removed exact model");
    }
    let fresh = reports.observation(&generation);
    assert_eq!(fresh, generation, "internal leases changed wire generation equality");
    reports.state_with_context("u1", values(&[("temp", json!(100))]), false, &fresh);
    reports.read_back_with_context("u1", values(&[("temp", json!(100))]), &fresh);
    assert_eq!(reports.values_of("u1").unwrap()["temp"], json!(100));
    assert_eq!(reports.snapshot()["u1"]["temp"], json!(100));
}

#[test]
fn contextual_removals_publish_the_mutations_cause_including_absence() {
    let a = "00-112233445566778899aabbccddeeff00-1234567890abcdef-01";
    let b = "00-222233445566778899aabbccddeeff00-1234567890abcdef-01";
    for cause in [Some(b), None] {
        let reports = Reports::new();
        let original = RequestContext::default().with_traceparent(Some(a));
        let mutation = RequestContext::default().with_traceparent(cause);
        reports.state_with_context("u1", values(&[("power", json!(false)), ("level", json!(10))]), false, &original);
        reports.take();
        reports.remove_property_with_context("u1", "level", &mutation);
        assert_eq!(reports.take().states[0].2.traceparent(), cause, "property removal retained the old cause");
        reports.assume_unproven_with_context("u1", values(&[("power", json!(true))]), &original);
        reports.take();
        assert!(reports.withdraw_assumption_with_context("u1", "power", &mutation));
        assert_eq!(reports.take().states[0].2.traceparent(), cause, "assumption withdrawal retained the old cause");
    }
}

#[test]
fn fenced_reports_and_removals_require_the_original_current_context() {
    let reports = Reports::new();
    reports.install_policy(RequestPolicy::Fenced(crate::TargetGenerations {
        instance: "epoch.2".into(),
        targets: [("u1".into(), "epoch.2".into())].into_iter().collect(),
    }));
    let fresh = RequestContext::new("epoch.2").unwrap();
    reports.state_with_context("u1", values(&[("power", json!(true))]), false, &fresh);
    for context in [RequestContext::default(), RequestContext::new("epoch.1").unwrap()] {
        reports.state_with_context("u1", values(&[("power", json!(false))]), false, &context);
        reports.remove_with_context("u1", &context);
        reports.remove_property_with_context("u1", "power", &context);
        reports.read_back_with_context("u1", values(&[("power", json!(false))]), &context);
        reports.written_with_context("u1", &values(&[("power", json!(false))]), 0, &context);
        reports.observed_event(Some("u1"), Outbound { context, topic: None, payload: b"{}".to_vec(), correlation: None });
    }
    assert_eq!(reports.snapshot()["u1"]["power"], json!(true), "late observations changed a fresh mapping");
    assert!(reports.read_back_view().1.is_empty(), "stale retained state entered the conflict basis");
    let changes = reports.take();
    assert_eq!(changes.states[0].2, fresh, "state lost its original generation");
    assert!(changes.events.is_empty(), "stale input event was delivered");
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

#[test]
fn optional_context_does_not_erase_unfenced_partial_state() {
    let reports = Reports::new();
    reports.install_policy(RequestPolicy::Unfenced);
    reports.state("u1", values(&[("power", json!(true)), ("level", json!(10))]), false);
    reports.state_with_context("u1", values(&[("level", json!(20))]), true, &RequestContext::new("optional.1").unwrap());
    assert_eq!(
        reports.snapshot()["u1"],
        values(&[("power", json!(true)), ("level", json!(20))]),
        "optional context erased unrelated legacy state"
    );
    reports.state("u1", values(&[("power", json!(false))]), false);
    assert_eq!(
        reports.snapshot()["u1"],
        values(&[("power", json!(false)), ("level", json!(20))]),
        "a partial legacy report erased another property"
    );
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
    assert_eq!(model.states.iter().map(|(t, _, _)| t.as_str()).collect::<Vec<_>>(), ["a", "b"]);
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
        reports.event(Outbound {
            context: crate::RequestContext::default(),
            topic: None,
            payload: format!("{i}").into_bytes(),
            correlation: None,
        });
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
    assert!(model.states.iter().all(|(t, _, _)| t != "a#b"), "a target that is no topic segment reached the model: {model:?}");
    let t2: Value = serde_json::from_slice(&model.states.iter().find(|(t, _, _)| t == "t2").expect("t2").1).unwrap();
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
        context: crate::RequestContext::default(),
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
/// arrives after an incomplete read-back never changes the "before" values in mid-process
/// (Claude's re-gate minor 11).
#[test]
fn a_late_state_does_not_change_the_read_back_basis() {
    let reports = Reports::new();
    reports.read_back("u1", values(&[("level", json!(10))]));
    reports.read_back_incomplete();
    reports.read_back("u1", values(&[("level", json!(40))]));
    assert_eq!(reports.read_back_before().1["u1"]["level"], json!(10), "a late State changed the conflict rule's read-back");
    assert_eq!(reports.broker_basis("u1", "level").before, Some(json!(10)));
    assert!(!reports.read_back_is_complete());
    reports.read_back_complete();
    assert!(reports.read_back_is_complete());
    assert_eq!(reports.read_back_before().1["u1"]["level"], json!(10), "completing it late changed the frozen read-back");
}

/// A driver's read of the read-back is one snapshot: never `complete` with a map the completion
/// had not frozen yet (third confirming reviews, Codex 3 and Claude F1: the map and the flag were
/// read apart, and the flag set before the freeze — 7 torn reads in 20000). The reviewer's probe:
/// a reader spinning on `read_back()` while the last State lands and the read-back completes.
#[test]
fn a_read_back_is_never_complete_with_a_partial_map() {
    use std::sync::atomic::{AtomicBool, Ordering};
    const N: usize = 40;
    let mut torn = 0usize;
    for _ in 0..20000 {
        let reports = Reports::new();
        for i in 0..N - 1 {
            reports.read_back(&format!("t{i}"), values(&[("p", json!(i))]));
        }
        let reader = crate::link::Reporter::new(reports.clone());
        let stop = Arc::new(AtomicBool::new(false));
        let go = Arc::new(AtomicBool::new(false));
        let (stopped, started) = (stop.clone(), go.clone());
        let spinning = std::thread::spawn(move || {
            let mut bad = 0usize;
            started.store(true, Ordering::SeqCst);
            while !stopped.load(Ordering::SeqCst) {
                let read = reader.read_back();
                if read.complete && read.targets.len() < N {
                    bad += 1;
                }
            }
            bad
        });
        while !go.load(Ordering::SeqCst) {
            std::hint::spin_loop();
        }
        reports.read_back("last", values(&[("p", json!(1))]));
        reports.read_back_complete();
        for _ in 0..3000 {
            std::hint::spin_loop();
        }
        stop.store(true, Ordering::SeqCst);
        torn += spinning.join().expect("the reader panicked");
    }
    assert_eq!(torn, 0, "a read-back read as complete with a partial map: {torn} torn reads");
}

/// Several devices' reports share the session's readiness signal (multi-device spec §4.7): a report
/// in any of them wakes the one session that serves them all.
#[tokio::test]
async fn reports_sharing_a_signal_wake_one_session() {
    let ready = Arc::new(Notify::new());
    let (a, b) = (Reports::with_ready(ready.clone()), Reports::with_ready(ready.clone()));
    b.status(LinkStatus::Connecting);
    let woke = tokio::time::timeout(std::time::Duration::from_secs(1), ready.notified()).await;
    assert!(woke.is_ok(), "a report in the second device's reports did not wake the shared signal");
    assert!(b.has_untaken() && !a.has_untaken(), "what waits is not told per device");
    let _ = b.take();
    assert!(!b.has_untaken(), "a take left something waiting");
}

/// The owner's healing of its slot (multi-device spec §4.5): a foreign write to its `Status` or a
/// target's `State` makes it publish its current value again — once however many writes came
/// (coalesced) — and a target it never reported is left as it is.
#[test]
fn a_reasserted_slot_is_published_again_once() {
    let reports = Reports::new();
    assert!(!reports.reassert_status(), "a status never reported was reasserted");
    reports.status(LinkStatus::Connected);
    reports.state("t1", values(&[("power", json!(true))]), false);
    let _ = reports.take();
    assert!(reports.reassert_status() && reports.reassert_status(), "the status was not reasserted");
    assert!(reports.reassert_state("t1") && reports.reassert_state("t1"), "a reported target was not reasserted");
    assert!(!reports.reassert_state("t9"), "a target never reported was reasserted");
    let changes = reports.take();
    assert_eq!(changes.status, Some(LinkStatus::Connected), "the reasserted status was not published again");
    assert_eq!(changes.states.len(), 1, "two reassertions of one target were not coalesced: {:?}", changes.states);
    assert_eq!(serde_json::from_slice::<Value>(&changes.states[0].1).unwrap(), json!({"power": true}));
    assert_eq!(reports.take(), Changes::default(), "a reassertion was published twice");
}

#[test]
fn offline_retired_names_do_not_accumulate_without_bound() {
    let reports = Reports::new();
    for i in 0..1200 {
        let target = format!("OldLight{i}");
        reports.state(&target, values(&[("power", json!(true))]), false);
        reports.remove(&target);
    }
    assert!(reports.locked().changed.len() <= 1024);
    let model = reports.model();
    assert!(model.states.len() <= 1024, "retired names grew to {}", model.states.len());
    let strings: usize = model.states.iter().map(|(name, _, _)| name.len()).sum();
    assert!(strings <= 128 * 1024, "retired-name storage grew to {strings} bytes");
    assert!(reports.take().states.is_empty());
}

#[test]
fn retired_names_wait_for_their_publish_acknowledgement() {
    let reports = Reports::new();
    reports.remove("Light1");
    let nonce = reports.take_sealed().retractions["Light1"];
    reports.retire_acknowledged_removals(100);
    assert_eq!(reports.model().states.len(), 1, "enqueue was never confirmed");
    reports.removal_sent("Light1", nonce, 101);
    reports.retire_acknowledged_removals(100);
    assert_eq!(reports.model().states.len(), 1, "an earlier PUBACK cleared this removal");
    reports.retire_acknowledged_removals(101);
    assert!(reports.model().states.is_empty());
    assert!(!reports.removals_inflight());
}

#[test]
fn retired_name_receipts_cannot_clear_a_new_removal_and_reconnect_retries() {
    let reports = Reports::new();
    reports.remove("Light1");
    let old = reports.take_sealed().retractions["Light1"];
    reports.state("Light1", values(&[("power", json!(true))]), false);
    reports.remove("Light1");
    let new = reports.take_sealed().retractions["Light1"];
    assert_ne!(old, new);
    reports.removal_sent("Light1", old, 1);
    reports.retire_acknowledged_removals(1);
    assert_eq!(reports.model_sealed().retractions["Light1"], new);
    reports.removal_sent("Light1", new, 2);
    reports.removals_connection_started();
    reports.retire_acknowledged_removals(10);
    assert_eq!(reports.model_sealed().retractions["Light1"], new, "old connection's sequence was reused");
    reports.removal_sent("Light1", new, 3);
    reports.retire_acknowledged_removals(3);
    assert!(reports.model().states.is_empty());
}

#[test]
fn offline_retired_name_bytes_and_changed_queue_are_bounded() {
    let reports = Reports::new();
    for i in 0..100 {
        reports.remove(&format!("{i}{}", "x".repeat(5000)));
    }
    assert!(reports.locked().changed.len() < 100);
    let model = reports.model();
    assert!(model.states.iter().map(|(name, _, _)| name.len()).sum::<usize>() <= 128 * 1024);
    assert!(model.states.iter().any(|(name, _, _)| name.starts_with("99x")));
    reports.remove(&"x".repeat(129 * 1024));
    assert!(!reports.locked().changed.iter().any(|name| name.len() > 128 * 1024));
}

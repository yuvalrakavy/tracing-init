//! Driver errors (Bridge Runtime spec §4.3, §9): `Rejected`, `Unusable` and `Link` with their
//! behaviours — reconnects paced at most one per retry interval, the outage one episode with proof
//! of life before recovery; unacknowledged writes held again when the link dies before a probe; a
//! driver call that never returns, a lost link within its deadline; a driver that panics.

use std::time::Duration;

use mqtt_bridge_conformance::*;
use mqtt_bridge_kit::{Feedback, TimingOverrides, Writes};
use serde_json::{json, Value};

const SETTLE: Duration = Duration::from_millis(600);

fn values(pairs: &[(&str, Value)]) -> Value {
    Value::Object(pairs.iter().map(|(k, v)| ((*k).to_owned(), v.clone())).collect())
}

fn up_at_20(_: &FakeBroker, _: &mqtt_bridge_kit::topics::Topics, device: &Device) {
    device.set("t1", "temp", json!(20));
}

async fn connected(h: &Harness) -> bool {
    h.eventually(BOUND, |h| h.status().as_deref() == Some("connected")).await
}

const REQ: Req = Req { expiry: Some(60), conflict: Some("DeviceWins") };

/// `Rejected`: an `Error` naming the target and the property (the Store abandons the request), one
/// WARN `command_rejected`; the link stays up.
#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_request_is_reported_and_the_link_stays_up() {
    let h = Harness::start_with("rejected", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer(Op::Apply, None, Answer::Rejected);
    assert!(h.desire("t1", "temp", json!(22), REQ));
    assert!(h.eventually(BOUND, |h| !h.errors_for("rejected").is_empty()).await, "the rejection got no Error: {:?}", h.errors());
    let rejected = &h.errors_for("rejected")[0];
    assert_eq!((rejected["target"].as_str(), rejected["property"].as_str()), (Some("t1"), Some("temp")));
    let warns = h.log.of_kind("command_rejected");
    assert!(warns.len() == 1 && warns[0].level == tracing::Level::WARN, "not one WARN `command_rejected`: {:#?}", h.log.records());
    h.settle(SETTLE).await;
    assert_eq!(h.device.calls_of(Op::Connect).len(), 1, "a rejection was taken for a lost link");
    assert_eq!(h.status().as_deref(), Some("connected"));
    assert!(h.stop().await);
}

/// `Unusable`: an `Error` once, the request passed over — never retried, never a reconnect for it
/// (no-hang 3b: reconnecting for it was a hot loop).
#[tokio::test(flavor = "multi_thread")]
async fn an_unusable_answer_is_reported_once_and_passed_over() {
    let h = Harness::start_with("unusable", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer(Op::Apply, None, Answer::Unusable);
    assert!(h.desire("t1", "temp", json!(22), REQ));
    assert!(h.eventually(BOUND, |h| !h.errors_for("unusable").is_empty()).await, "the unusable answer got no Error: {:?}", h.errors());
    h.settle(SETTLE).await;
    assert_eq!(h.errors_for("unusable").len(), 1, "the Error was published more than once");
    assert_eq!(h.device.calls_of(Op::Apply).len(), 1, "an unusable answer was retried");
    assert_eq!(h.device.calls_of(Op::Connect).len(), 1, "an unusable answer was taken for a lost link");
    assert!(h.stop().await);
}

/// `Link`: a device that takes each connection and fails on it is reconnected to at most once per
/// retry interval; its outage is one episode — INFO `device_connection_lost`, one WARN
/// `device_unreachable` past the threshold, no recovery until a read succeeds (proof of life, never
/// a bare connect) — then INFO `device_recovered` with `down_for_ms` and `attempts`.
#[tokio::test(flavor = "multi_thread")]
async fn a_lost_link_is_reconnected_at_the_paced_rate_and_its_outage_is_one_episode() {
    let timing = TimingOverrides { outage_warn: Some(Duration::from_millis(900)), ..quick_timing() };
    let h = Harness::start_with("paced", acknowledged(), timing, up_at_20).await;
    assert!(connected(&h).await);
    // Every connect succeeds, and every read on it fails: a device that accepts and drops.
    h.device.answer(Op::Read, None, Answer::Link);
    h.device.set_up(false);
    h.device.set_up(true);
    h.settle(Duration::from_millis(1800)).await;
    let connects: Vec<std::time::Instant> = h.device.calls_of(Op::Connect).iter().map(|c| c.at).collect();
    assert!(connects.len() >= 4, "the device was not reconnected to: {} connects", connects.len());
    for pair in connects.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_millis(280), "two connects {gap:?} apart: reconnects are not paced at the retry interval (300 ms)");
    }
    assert_eq!(h.log.of_kind("device_connection_lost").len(), 1, "the outage began more than once: {:#?}", h.log.records());
    assert!(h.log.of_kind("device_recovered").is_empty(), "a bare connect was taken for recovery: {:#?}", h.log.records());
    let warned = h.log.of_kind("device_unreachable");
    assert!(
        warned.len() == 1 && warned[0].level == tracing::Level::WARN,
        "the outage past its threshold was not one WARN: {:#?}",
        h.log.records()
    );

    h.device.answer_normally();
    assert!(h.eventually(BOUND, |h| !h.log.of_kind("device_recovered").is_empty()).await, "the outage never ended");
    let recovered = &h.log.of_kind("device_recovered")[0];
    assert!(recovered.field("down_for_ms").is_some() && recovered.field("attempts").is_some_and(|a| a.parse::<u32>().unwrap_or(0) >= 4));
    assert!(connected(&h).await);
    assert!(h.stop().await);
}

/// `Status` says `connected` only once the device has proved itself alive: a device that takes the
/// connection and then answers nothing stays `unreachable` (the link goes `Connecting` →
/// `Proving` → `Up`, and `Proving` is not up).
#[tokio::test(flavor = "multi_thread")]
async fn status_says_connected_only_once_the_device_proved_alive() {
    let h = Harness::start_with("proving", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    let connected_said = |h: &Harness| h.published(&h.topics.status()).iter().filter(|r| &r.payload[..] == b"connected").count();
    let before = connected_said(&h);
    // Every connect succeeds, and every read on it goes unanswered until its deadline.
    h.device.answer(Op::Read, None, Answer::Hang);
    h.device.set_up(false);
    h.device.set_up(true);
    h.settle(Duration::from_millis(1500)).await;
    assert!(h.device.calls_of(Op::Connect).len() >= 3, "the device was not reconnected to");
    assert_eq!(connected_said(&h), before, "Status said connected for a device that never proved itself alive");
    assert_eq!(h.status().as_deref(), Some("unreachable"));
    h.device.answer_normally();
    assert!(connected(&h).await, "Status never said connected once the device answered");
    assert!(h.stop().await);
}

/// `Writes::Unacknowledged`: an applied value stays unproven until a later probe or read; a device
/// that took the write without acting on it, and leaves the probe unanswered, loses the link — and
/// the value is held again and applied on the next link (mqtt_lutron; the fleet's F5).
#[tokio::test(flavor = "multi_thread")]
async fn unacknowledged_writes_are_held_again_when_the_link_dies_before_a_probe() {
    let h = Harness::start_with("unproven", info(Writes::Unacknowledged, Feedback::Observed), quick_timing(), |_, _, device| {
        device.set("t1", "level", json!(0));
    })
    .await;
    assert!(connected(&h).await);
    h.device.set_deaf(true);
    assert!(h.desire("t1", "level", json!(40), REQ));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Probe).is_empty()).await, "the write was never probed: {:?}", h.device.calls());
    h.device.set_deaf(false);
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "level") == Some(json!(40))).await,
        "the unproven write was not held again: {:?}",
        h.device.calls()
    );
    assert_eq!(
        h.device.applies(),
        vec![("t1".to_owned(), values(&[("level", json!(40))])), ("t1".to_owned(), values(&[("level", json!(40))]))]
    );
    assert!(h.stop().await);
}

/// A driver call that never returns becomes a lost link within its deadline: the link drops, the
/// device is reconnected to, and the request — held again — is applied.
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_call_that_never_returns_is_a_lost_link_within_its_deadline() {
    let h = Harness::start_with("hang", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Apply, None, Answer::Hang, 1);
    assert!(h.desire("t1", "temp", json!(22), REQ));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Disconnect).is_empty()).await, "the hanging call never lost the link");
    let hung = h.device.calls_of(Op::Apply)[0].at;
    let dropped = h.device.calls_of(Op::Disconnect)[0].at;
    assert!(
        dropped - hung < Duration::from_millis(1500),
        "the link was lost {:?} after the call, past its 500 ms deadline",
        dropped - hung
    );
    assert!(h.eventually(BOUND, |h| h.device.get("t1", "temp") == Some(json!(22))).await, "the request in the hanging call was lost");
    assert!(h.stop().await);
}

/// A driver that panics ends the device worker; the bridge sees its death (ERROR `worker_died` and
/// a failure status, in a process).
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_panic_ends_the_device_worker() {
    let mut h = Harness::start_with("panic", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer(Op::Apply, None, Answer::Panic);
    assert!(h.desire("t1", "temp", json!(22), REQ));
    let died = tokio::time::timeout(BOUND, h.running().ended()).await.expect("the driver's panic went unseen");
    assert_eq!(died.part, "device");
    // With the worker's end unseen, the blocked-thread watchdog still ends the bridge, past the
    // operation's deadline and its grace: the panic itself went unseen.
    assert!(died.cause.contains("scripted panic"), "the driver's panic went unseen; the bridge saw {died:?}");
    assert!(h.stop().await);
}

/// A driver that blocks its thread — a synchronous call with no timeout — stops the worker's own
/// timers, so its operation's deadline cannot fire there. The watchdog on the MQTT side reads that
/// deadline: past it and a grace, the device is unreachable, the waiting momentary commands are
/// refused, and the device worker is dead — the bounded shutdown and a failure status, for the
/// service manager to restart the bridge (review M3).
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_that_blocks_its_thread_is_a_dead_worker_past_its_deadline() {
    let mut h = Harness::start_with("blocked", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Apply, None, Answer::Block(Duration::from_secs(10)), 1);
    assert!(h.desire("t1", "temp", json!(22), REQ));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Apply).is_empty()).await, "the apply never began");
    // Waiting behind the blocked apply.
    assert!(h.command(json!({"target": "t1", "command": "pulse"}), Some(("Store/r", b"p1"))));
    let blocked_at = h.device.calls_of(Op::Apply)[0].at;
    // The 500 ms deadline, the 2 s grace, a wide margin: well short of the 10 s the driver blocks.
    // The watchdog is read in short slices, as an embedding application's housekeeping loop
    // would: it runs on its own, and a slice that ends does not restart it (re-gate R1).
    let died = loop {
        if let Ok(died) = tokio::time::timeout(Duration::from_millis(50), h.running().ended()).await {
            break died;
        }
        assert!(
            blocked_at.elapsed() < Duration::from_secs(7),
            "a driver blocking its thread went unseen for 7 s, read in slices; Status {:?}",
            h.status()
        );
    };
    assert_eq!(died.part, "device", "{died:?}");
    assert!(died.cause.contains("blocked"), "{died:?}");
    assert!(blocked_at.elapsed() < Duration::from_secs(8), "seen only once the driver let go: {:?}", blocked_at.elapsed());
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await, "Status still says {:?}", h.status());
    assert!(
        h.eventually(BOUND, |h| h.errors_for("refused").iter().any(|e| e["command"] == "pulse")).await,
        "the momentary command waiting behind the blocked worker was not refused: {:?}",
        h.errors()
    );
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "the refused command was not answered");
    assert!(h.stop().await);
}

/// A helper the link cannot run without — an unsolicited-input reader — that ends is a lost link:
/// one that returns (end of file, given up without a word), and one that panics, which is also a
/// driver bug, ERROR `driver_contract_violation` (review M5). Unwatched, a listening driver stayed
/// "connected" and deaf.
#[tokio::test(flavor = "multi_thread")]
async fn a_helper_that_ends_is_a_lost_link() {
    for helper in [Helper::ReturnAfter(Duration::from_millis(200)), Helper::PanicAfter(Duration::from_millis(200))] {
        let h = Harness::start_with("helper", acknowledged(), quick_timing(), |_, _, device| {
            device.set("t1", "temp", json!(20));
            device.helper(helper);
        })
        .await;
        assert!(connected(&h).await);
        assert!(
            h.eventually(Duration::from_secs(3), |h| h.device.calls_of(Op::Connect).len() >= 2).await,
            "{helper:?}: the helper's end went unseen: {:?}",
            h.device.calls().iter().map(|c| c.op).collect::<Vec<_>>()
        );
        let lost = h.log.of_kind("device_connection_lost");
        assert!(lost.len() == 1 && lost[0].field("error").is_some_and(|e| e.contains("helper")), "{helper:?}: {:#?}", h.log.records());
        let violations = h.log.of_kind("driver_contract_violation");
        match helper {
            Helper::PanicAfter(_) => assert!(
                violations.len() == 1 && violations[0].level == tracing::Level::ERROR,
                "a helper's panic was not one ERROR: {:#?}",
                h.log.records()
            ),
            _ => assert!(violations.is_empty(), "a helper that returned was taken for a bug: {violations:#?}"),
        }
        assert!(connected(&h).await, "the device was not connected again");
        assert!(h.stop().await);
    }
}

/// A poll slower than its interval does not starve the device's other work: the next poll is due
/// an interval after the last one ended, and the mailbox is served between polls (review C4: due
/// from the last one's start, every poll was due at once, and no write ever ran).
#[tokio::test(flavor = "multi_thread")]
async fn a_poll_slower_than_its_interval_does_not_starve_writes() {
    // Operations may take 2 s here, so a loaded machine cannot turn the slow reads into lost links.
    let timing =
        TimingOverrides { poll: Some(Some(Duration::from_millis(300))), operation: Some(Duration::from_secs(2)), ..quick_timing() };
    let h = Harness::start_with("slowpoll", acknowledged(), timing, |_, _, device| {
        device.set("t1", "temp", json!(20));
        // Each read takes 400 ms: longer than the interval, well within the 2 s bound.
        device.answer(Op::Read, None, Answer::Delay(Duration::from_millis(400)));
    })
    .await;
    assert!(connected(&h).await);
    // Polls running back to back by now.
    h.settle(Duration::from_millis(1000)).await;
    assert!(h.desire("t1", "temp", json!(22), REQ));
    assert!(
        h.eventually(Duration::from_secs(3), |h| h.device.get("t1", "temp") == Some(json!(22))).await,
        "with polls slower than their interval, a write never ran: {} reads, {} applies",
        h.device.calls_of(Op::Read).len(),
        h.device.calls_of(Op::Apply).len()
    );
    assert_eq!(h.device.calls_of(Op::Connect).len(), 1, "a slow read lost the link: the test proves nothing about starvation");
    assert!(h.stop().await);
}

/// The next poll is due an interval after the last one ended, not after it began: a poll slower
/// than its interval does not run back to back (review C4; the re-gate split its control). Load
/// can only lengthen the gaps, so it cannot fail this falsely.
#[tokio::test(flavor = "multi_thread")]
async fn polls_are_spaced_an_interval_after_the_last_one_ended() {
    let timing =
        TimingOverrides { poll: Some(Some(Duration::from_millis(400))), operation: Some(Duration::from_secs(2)), ..quick_timing() };
    let h = Harness::start_with("pollgap", acknowledged(), timing, |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.answer(Op::Read, None, Answer::Delay(Duration::from_millis(600)));
    })
    .await;
    assert!(connected(&h).await);
    let first = h.device.calls_of(Op::Read).len();
    h.settle(Duration::from_millis(3500)).await;
    let starts: Vec<std::time::Instant> = h.device.calls_of(Op::Read).iter().skip(first).map(|c| c.at).collect();
    assert!(starts.len() >= 2, "too few polls to measure: {}", starts.len());
    for pair in starts.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(
            gap >= Duration::from_millis(900),
            "two polls started {gap:?} apart: the next poll was not due an interval after the last ended (600 ms + 400 ms)"
        );
    }
    assert!(h.stop().await);
}

/// A target or property name a driver reports that cannot stand as one topic segment (`a#b`,
/// `a/b`) is a driver bug: dropped, with an ERROR `driver_contract_violation`, and the session goes
/// on — after a reconnect too (review B2: rumqttc refused its `State` publish, the CONNACK duties
/// stopped there, and the session never published a report again).
#[tokio::test(flavor = "multi_thread")]
async fn a_reported_name_that_is_no_topic_segment_is_dropped_and_the_session_goes_on() {
    let h = Harness::start_with("badname", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set("a#b", "x", json!(1));
        device.set("t2", "a/b", json!(1));
        device.set("t2", "ok", json!(2));
    })
    .await;
    assert!(connected(&h).await);
    assert!(h.eventually(BOUND, |h| h.state("t2").is_some()).await, "the target beside the bad name was not published");
    let connects = h.broker.connections();
    h.broker.drop_connection();
    assert!(h.eventually(BOUND, |h| h.broker.connections() > connects && h.subscribed_again()).await, "the bridge did not reconnect");
    h.device.set("t1", "temp", json!(25));
    assert!(h.command(json!({"target": "t1", "command": "refresh"}), Some(("Store/r", b"1"))));
    assert!(
        h.eventually(BOUND, |h| h.state("t1").is_some_and(|s| s["temp"] == 25)).await,
        "after a reconnect the session published nothing: state(t1) = {:?}",
        h.state("t1")
    );
    assert!(h.eventually(BOUND, |h| !h.published("Store/r").is_empty()).await, "the refresh was not answered");
    assert_eq!(h.state("t2"), Some(json!({"ok": 2})), "the bad property was published, or the good one dropped");
    let violations = h.log.of_kind("driver_contract_violation");
    assert!(
        violations.len() == 2 && violations.iter().all(|v| v.level == tracing::Level::ERROR),
        "not one ERROR per bad name: {:#?}",
        h.log.records()
    );
    assert!(h.stop().await);
}

/// A driver with unacknowledged writes whose probe answers `Unsupported` proves nothing — it sent
/// nothing to the device — and breaks its contract: one ERROR `driver_contract_violation`, however
/// many probes. Its writes are still taken as proven, so it cannot become a probe hot loop
/// (review C11b).
#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_probe_of_an_unacknowledged_driver_is_one_contract_violation() {
    let h = Harness::start_with("noprobe", info(Writes::Unacknowledged, Feedback::Observed), quick_timing(), |_, _, device| {
        device.set("t1", "level", json!(0));
        device.answer(Op::Probe, None, Answer::Unsupported);
    })
    .await;
    assert!(connected(&h).await);
    for level in [40, 50] {
        assert!(h.desire("t1", "level", json!(level), REQ));
        assert!(h.eventually(BOUND, |h| h.device.get("t1", "level") == Some(json!(level))).await);
        h.settle(Duration::from_millis(300)).await;
    }
    assert!(h.device.calls_of(Op::Probe).len() >= 2, "the writes were not probed: {:?}", h.device.calls());
    assert!(h.device.calls_of(Op::Probe).len() < 10, "a probe loop: {} probes", h.device.calls_of(Op::Probe).len());
    let violations = h.log.of_kind("driver_contract_violation");
    assert!(
        violations.len() == 1 && violations[0].level == tracing::Level::ERROR,
        "an unsupported probe of an unacknowledged driver was not one ERROR: {:#?}",
        h.log.records()
    );
    assert_eq!(h.device.calls_of(Op::Connect).len(), 1, "an unsupported probe lost the link");
    assert!(h.stop().await);
}

/// An `apply` that wrote some values and refused one says so property by property
/// (`OpError::Partial`): the `Error` names only the refused property, and the rest stand as
/// applied — so the Store abandons only what failed (the mqtt_ac v2 build: the CoolMaster sends each
/// property as a command of its own and rolls nothing back).
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_apply_reports_only_the_properties_that_failed() {
    let h = Harness::start_with("partial", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set("t1", "power", json!(false));
    })
    .await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Apply, None, Answer::RefuseProperty("temp"), 1);
    assert!(h.desire_many("t1", json!({"power": true, "temp": 22}), REQ));
    assert!(h.eventually(BOUND, |h| !h.errors_for("rejected").is_empty()).await, "the refused property got no Error: {:?}", h.errors());
    h.settle(SETTLE).await;
    let rejected = h.errors_for("rejected");
    assert_eq!(
        rejected.iter().map(|e| e["property"].as_str()).collect::<Vec<_>>(),
        [Some("temp")],
        "the Error named more than the refused property: {rejected:?}"
    );
    assert_eq!(h.device.get("t1", "power"), Some(json!(true)));
    assert_eq!(h.device.get("t1", "temp"), Some(json!(20)));
    assert_eq!(h.device.calls_of(Op::Connect).len(), 1, "a partial outcome was taken for a lost link");
    assert!(h.stop().await);
}

/// Of a partial apply to a device with unacknowledged writes, the values written stay unproven like
/// any write: when the link dies before a probe proves them, they are held again and applied on the
/// next link (the R1 re-gate's R10: no test saw that accounting).
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_applys_written_values_are_held_again_when_the_link_dies_before_proof() {
    let h = Harness::start_with("partial-unproven", info(Writes::Unacknowledged, Feedback::Observed), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.set("t1", "power", json!(false));
    })
    .await;
    assert!(connected(&h).await);
    h.device.set_deaf(true);
    h.device.answer_times(Op::Apply, None, Answer::RefuseProperty("temp"), 1);
    let req = Req { expiry: Some(3600), conflict: Some("RequestWins") };
    assert!(h.desire_many("t1", json!({"power": true, "temp": 22}), req));
    assert!(h.eventually(BOUND, |h| !h.device.calls_of(Op::Apply).is_empty()).await, "the write never reached the device");
    h.settle(Duration::from_millis(100)).await;
    // The link dies before anything proves the write (if the probe has not lost it already).
    h.device.set_up(false);
    h.device.set_deaf(false);
    h.device.set_up(true);
    assert!(
        h.eventually(BOUND, |h| h.device.get("t1", "power") == Some(json!(true))).await,
        "the value a partial apply wrote was not held again when the link died before proof: {:?}",
        h.device.calls()
    );
    assert!(h.stop().await);
}

/// A helper that blocks the worker's thread while no operation runs — a reader calling a
/// synchronous read without `blocking()` — is a dead worker: no deadline is set then, so the
/// watchdog reads the worker runtime's heartbeat, which a blocked thread stops (the R1 re-gate's R1:
/// the watchdog saw only an operation's deadline, and the bridge stayed "connected" and deaf).
#[tokio::test(flavor = "multi_thread")]
async fn a_helper_that_blocks_the_worker_thread_while_idle_is_a_dead_worker() {
    let mut h = Harness::start_with("idle-block", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.helper(Helper::BlockAfter(Duration::from_millis(300), Duration::from_secs(15)));
    })
    .await;
    assert!(connected(&h).await);
    let started = std::time::Instant::now();
    let died = tokio::time::timeout(Duration::from_secs(10), h.running().ended()).await;
    let died = died.unwrap_or_else(|_| panic!("a helper blocking the idle worker's thread went unseen for 10 s; Status {:?}", h.status()));
    assert_eq!(died.part, "device", "{died:?}");
    assert!(died.cause.contains("blocked"), "{died:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "seen only once the helper let go: {:?}", started.elapsed());
    assert!(h.eventually(BOUND, |h| h.status().as_deref() == Some("unreachable")).await, "Status still says {:?}", h.status());
    assert!(h.stop().await, "the bridge did not stop within its bound with its worker blocked");
}

/// Blocking calls that fill their cap and stay stuck — readers whose synchronous reads never return
/// — make every later call a refusal, for good: past twice the operation bound, that is a dead
/// worker, and the restart the only way out (the R1 re-gate's R13: nothing escalated).
#[tokio::test(flavor = "multi_thread")]
async fn a_blocking_cap_that_stays_full_is_a_dead_worker() {
    let mut h = Harness::start_with("cap-full", acknowledged(), quick_timing(), |_, _, device| {
        device.set("t1", "temp", json!(20));
        device.helper(Helper::HoldBlocking);
    })
    .await;
    assert!(connected(&h).await);
    let died = tokio::time::timeout(Duration::from_secs(8), h.running().ended()).await;
    let died = died.unwrap_or_else(|_| panic!("a blocking cap full for good went unseen for 8 s; Status {:?}", h.status()));
    assert_eq!(died.part, "device", "{died:?}");
    assert!(died.cause.contains("cap"), "{died:?}");
    assert!(h.stop().await);
}

/// A `disconnect` that blocks the worker's thread — a synchronous close with no timeout — is
/// watched like an operation: past its bound and the grace, the worker is dead, said for what it
/// is (the R1 re-gate's T4: no test covered the clock set around the disconnect).
#[tokio::test(flavor = "multi_thread")]
async fn a_disconnect_that_blocks_the_worker_thread_is_a_dead_worker_past_its_bound() {
    let mut h = Harness::start_with("disconnect-block", acknowledged(), quick_timing(), up_at_20).await;
    assert!(connected(&h).await);
    h.device.answer_times(Op::Disconnect, None, Answer::Block(Duration::from_secs(15)), 1);
    let lost_at = std::time::Instant::now();
    h.device.set_up(false);
    let died = tokio::time::timeout(Duration::from_secs(10), h.running().ended()).await;
    let died = died.unwrap_or_else(|_| panic!("a disconnect blocking the worker's thread went unseen for 10 s; Status {:?}", h.status()));
    assert_eq!(died.part, "device", "{died:?}");
    assert!(died.cause.contains("deadline"), "the blocked disconnect was not seen past its bound: {died:?}");
    assert!(lost_at.elapsed() < Duration::from_secs(10), "seen only once the disconnect let go: {:?}", lost_at.elapsed());
    assert!(h.stop().await);
}

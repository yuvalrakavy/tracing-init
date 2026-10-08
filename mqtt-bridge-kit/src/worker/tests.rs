//! The worker's decisions that need no device: the outage conflict rule, and the outage episode.

use serde_json::json;

use super::*;
use crate::test_support::Capture;

fn request(value: Value, conflict: Conflict) -> StateValue {
    StateValue { target: "u1".into(), property: "temp".into(), value, deadline: None, conflict, check: Default::default(), basis: None }
}

/// The outage conflict rule (Store kit spec §4.5), every row: unchanged → applied; changed and
/// `DeviceWins` → superseded; changed and `RequestWins` → applied; no "before" value → applied;
/// the device already at the requested value → nothing to write; the device's value unknown →
/// applied under `RequestWins`, and under `DeviceWins` not applied on a guess (review M6).
#[test]
fn the_outage_conflict_rule() {
    let (b, c) = (json!(20), json!(18));
    for conflict in [Conflict::DeviceWins, Conflict::RequestWins] {
        assert_eq!(resolve(&request(json!(22), conflict), Some(&b), Some(&b)), Resolution::Apply, "unchanged, {conflict:?}");
        assert_eq!(resolve(&request(json!(22), conflict), Some(&c), None), Resolution::Apply, "no before value, {conflict:?}");
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), Some(&b)), Resolution::Satisfied, "already there, {conflict:?}");
    }
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), None, Some(&b)), Resolution::Apply, "current unknown, RequestWins");
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), None, Some(&b)), Resolution::Unknown, "current unknown, DeviceWins");
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), Some(&c), Some(&b)), Resolution::Superseded);
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), Some(&c), Some(&b)), Resolution::Apply);
}

/// The rule's rows, in the order they are taken (the lead's F1, after the R1 re-gate): the first
/// that holds decides. With no "before" value — a first install — nothing can have changed, so
/// the request is applied even when the device cannot say its current value (kit spec §4.5).
#[test]
fn the_conflict_rule_takes_its_rows_in_order() {
    let (b, c) = (json!(20), json!(18));
    for conflict in [Conflict::DeviceWins, Conflict::RequestWins] {
        // 1. The current value known and the requested one: nothing to write, before or not.
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), Some(&b)), Resolution::Satisfied, "row 1, {conflict:?}");
        assert_eq!(resolve(&request(json!(18), conflict), Some(&c), None), Resolution::Satisfied, "row 1 with no before, {conflict:?}");
        // 2. No before value: applied, the current value known or not.
        assert_eq!(resolve(&request(json!(22), conflict), Some(&c), None), Resolution::Apply, "row 2, {conflict:?}");
        assert_eq!(
            resolve(&request(json!(22), conflict), None, None),
            Resolution::Apply,
            "row 2: a first install was not applied because the current value is unknown, {conflict:?}"
        );
        // 5. Unchanged since before: applied.
        assert_eq!(resolve(&request(json!(22), conflict), Some(&b), Some(&b)), Resolution::Apply, "row 5, {conflict:?}");
    }
    // 3. A before value, and the current one unknown: not applied on a guess under DeviceWins.
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), None, Some(&b)), Resolution::Unknown, "row 3, DeviceWins");
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), None, Some(&b)), Resolution::Apply, "row 3, RequestWins");
    // 4. Changed since before.
    assert_eq!(resolve(&request(json!(22), Conflict::DeviceWins), Some(&c), Some(&b)), Resolution::Superseded, "row 4, DeviceWins");
    assert_eq!(resolve(&request(json!(22), Conflict::RequestWins), Some(&c), Some(&b)), Resolution::Apply, "row 4, RequestWins");
}

/// A device outage is one episode: INFO `device_connection_lost` on its first failure, DEBUG
/// after, one WARN `device_unreachable` past its threshold, INFO `device_recovered` with its
/// counts.
#[test]
fn a_device_outage_is_one_episode() {
    let log = Capture::start();
    let mut outage = DeviceOutage::new(Duration::from_millis(50), Duration::ZERO);
    let error = LinkError::new("refused");
    outage.failed(&error);
    outage.failed(&error);
    std::thread::sleep(Duration::from_millis(60));
    outage.failed(&error);
    outage.failed(&error);
    outage.recovered(2, 1, 0);
    outage.stable();
    let kinds: Vec<(tracing::Level, String)> =
        log.records().into_iter().filter(|r| !r.kind.is_empty()).map(|r| (r.level, r.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (tracing::Level::INFO, "device_connection_lost".to_owned()),
            (tracing::Level::WARN, "device_unreachable".to_owned()),
            (tracing::Level::INFO, "device_recovered".to_owned()),
        ]
    );
    let recovered = log.of_kind("device_recovered");
    assert_eq!(
        (recovered[0].field("attempts"), recovered[0].field("applied"), recovered[0].field("refused")),
        (Some("4"), Some("2"), Some("1"))
    );
}

fn link_context() -> LinkContext {
    LinkContext::new(1, "i", Reports::new(), Arc::new(crate::link::Abandoned::default()), Arc::new(OpClock::new()))
}

/// An operation that has completed wins over a link loss noticed at the same moment: its result is
/// what the device did, and the loss is seen at the worker's next step. Raced unbiased, a helper's
/// `link_lost` turned a completed `Ok` into a `Link` error half the time (the mqtt_ac v2 build).
/// Here the loss comes while the operation runs, in the poll that completes it.
#[tokio::test]
async fn an_operation_that_completed_wins_over_a_link_lost_at_the_same_moment() {
    for _ in 0..64 {
        let ctx = link_context();
        let handle = ctx.handle();
        let losing = handle.clone();
        let result = op(&handle, Duration::from_secs(5), "apply", async move {
            losing.link_lost("lost meanwhile");
            Ok::<u32, OpError>(7)
        })
        .await;
        assert_eq!(result, Ok(7), "a completed operation was turned into a lost link");
        assert!(handle.is_lost(), "the loss itself was forgotten");
    }
}

/// An operation that completed and a loss reported while it was pending, both ready when the
/// operation is next polled: its result wins (item 34, the unbiased race: half the time a lost link).
#[tokio::test]
async fn a_completion_and_a_loss_ready_together_keep_the_result() {
    for _ in 0..64 {
        let ctx = link_context();
        let handle = ctx.handle();
        let (done, wait) = tokio::sync::oneshot::channel::<()>();
        let operation = op(&handle, Duration::from_secs(5), "apply", async move {
            let _ = wait.await;
            Ok::<u32, OpError>(7)
        });
        tokio::pin!(operation);
        // Polled once: admitted while the link is live, and pending.
        tokio::select! {
            biased;
            early = &mut operation => panic!("the operation ended before it was let go: {early:?}"),
            () = std::future::ready(()) => {}
        }
        handle.link_lost("lost meanwhile");
        let _ = done.send(());
        assert_eq!(operation.await, Ok(7), "a completed operation was turned into a lost link");
    }
}

/// A helper's blocking call has no deadline, even when it starts while an operation runs; the
/// operation's own blocking calls are held to its deadline (spec §4.2; the R1 re-gate's R14: the
/// deadline was the link's, so a reader's quiet read that overlapped a probe "overran" and lost
/// the link).
#[tokio::test(flavor = "multi_thread")]
async fn a_helpers_blocking_call_is_not_held_to_an_operation_it_overlapped() {
    let ctx = link_context();
    let handle = ctx.handle();
    let reader = handle.clone();
    let mut read = None;
    let probed = op(&handle, Duration::from_millis(100), "probe", async {
        read = Some(tokio::spawn(async move {
            reader
                .blocking(|| {
                    std::thread::sleep(Duration::from_millis(400));
                    "line"
                })
                .await
        }));
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok::<(), OpError>(())
    })
    .await;
    assert_eq!(probed, Ok(()));
    let read = read.expect("the helper started").await.expect("the helper ran");
    assert_eq!(read, Ok("line"), "a helper's read was held to the deadline of an operation it overlapped");
    assert!(!handle.is_lost(), "a helper's read lost the link");
    // The operation's own blocking call is held to its deadline.
    let ctx = link_context();
    let handle = ctx.handle();
    let inner = handle.clone();
    let slow = op(&handle, Duration::from_millis(100), "apply", async move {
        inner.blocking(|| std::thread::sleep(Duration::from_millis(400))).await.map_err(OpError::Link)
    })
    .await;
    assert!(matches!(slow, Err(OpError::Link(_))), "an operation's blocking call was not held to its deadline: {slow:?}");
}

/// An operation is not started on a link already lost: it is refused at once with the loss, and
/// the driver is not called (the R1 re-gate's R6: the biased wrapper ran every new operation on a
/// lost link, and catch-up or restore writes went on after the loss).
#[tokio::test]
async fn an_operation_is_not_started_on_a_link_already_lost() {
    let ctx = link_context();
    let handle = ctx.handle();
    handle.link_lost("lost before");
    let started = std::sync::atomic::AtomicBool::new(false);
    let result = op(&handle, Duration::from_secs(5), "apply", async {
        started.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok::<u32, OpError>(7)
    })
    .await;
    assert!(
        matches!(&result, Err(OpError::Link(e)) if e.reason().contains("lost before")),
        "an operation started on a link already lost: {result:?}"
    );
    assert!(!started.load(std::sync::atomic::Ordering::SeqCst), "the driver was called on a link already lost");
}

/// JSON numbers compare by value: `24` and `24.0` are the same value, so a held request the device
/// already has is satisfied, and an unchanged "before" value is unchanged (the mqtt_ac v2 build).
#[test]
fn json_numbers_compare_by_value() {
    let (int, float) = (json!(24), json!(24.0));
    assert_eq!(
        resolve(&request(json!(24), Conflict::DeviceWins), Some(&float), Some(&json!(20))),
        Resolution::Satisfied,
        "24 and 24.0 differ"
    );
    assert_eq!(
        resolve(&request(json!(26), Conflict::DeviceWins), Some(&int), Some(&float)),
        Resolution::Apply,
        "an unchanged before value counted as a change"
    );
    // Two integers compare exactly: through f64, 2^53 + 1 and 2^53 are one (re-gate minor).
    assert_eq!(
        resolve(&request(json!(9_007_199_254_740_993_u64), Conflict::DeviceWins), Some(&json!(9_007_199_254_740_992_u64)), None),
        Resolution::Apply,
        "two different integers compared as the same value"
    );
    assert_eq!(
        resolve(&request(json!(-9_007_199_254_740_993_i64), Conflict::DeviceWins), Some(&json!(-9_007_199_254_740_992_i64)), None),
        Resolution::Apply,
        "two different negative integers compared as the same value"
    );
}

/// A device that answers the proving read and drops right after its catch-up is one outage, not an
/// INFO pair per attempt and never the WARN: recovery is declared only once a recovered link has
/// lasted (review MINOR 10; the broker side's `stable_after`).
#[test]
fn a_device_that_drops_right_after_its_catch_up_is_one_outage() {
    let log = Capture::start();
    let mut outage = DeviceOutage::new(Duration::from_millis(50), Duration::from_millis(200));
    let error = LinkError::new("dropped after the catch-up");
    outage.failed(&error);
    outage.recovered(1, 0, 0);
    outage.failed(&error);
    std::thread::sleep(Duration::from_millis(60));
    outage.recovered(0, 0, 0);
    outage.failed(&error);
    let kinds: Vec<(tracing::Level, String)> =
        log.records().into_iter().filter(|r| !r.kind.is_empty()).map(|r| (r.level, r.kind)).collect();
    assert_eq!(
        kinds,
        vec![(tracing::Level::INFO, "device_connection_lost".to_owned()), (tracing::Level::WARN, "device_unreachable".to_owned())],
        "a link lost right after its catch-up was not one outage"
    );
    // A link that lasts ends it, once, with what every catch-up of it did.
    outage.recovered(2, 0, 0);
    outage.stable();
    assert!(log.of_kind("device_recovered").is_empty(), "recovered before the link had lasted");
    std::thread::sleep(Duration::from_millis(220));
    outage.stable();
    let recovered = log.of_kind("device_recovered");
    assert!(recovered.len() == 1 && recovered[0].field("applied") == Some("3"), "{:#?}", log.records());
}

/// A restore waits for the retained replays to go quiet — a config or a replayed request arriving
/// after the read-back is done restarts the window — however soon the read-back was done (the R1
/// re-gate's R5: a SUBACK is no proof they were handled).
#[tokio::test]
async fn a_restore_waits_for_the_replays_to_go_quiet() {
    let reports = Reports::new();
    let mailbox = Mailbox::new();
    reports.read_back_complete();
    let started = Instant::now();
    let posting = mailbox.clone();
    let poster = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let _ = posting.post_config("t1", Some(json!({"channel": 1})));
    });
    restore_ready(&reports, &mailbox, false).await;
    let waited = started.elapsed();
    let _ = poster.await;
    assert!(waited >= RESTORE_QUIET + Duration::from_millis(90), "the restore did not wait for the replays to go quiet: {waited:?}");
}

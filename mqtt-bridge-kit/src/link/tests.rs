//! The link context: a lost link said once, a blocking call bounded and its abandoned threads
//! capped.

use std::time::Duration;

use super::*;

fn context(generation: u64) -> LinkContext {
    LinkContext::new(generation, "i", Reports::new(), Arc::new(Abandoned::default()), Arc::new(OpClock::new()))
}

/// A helper's `link_lost` ends the worker's wait on it, with the first reason given.
#[tokio::test]
async fn a_lost_link_is_said_once_with_its_first_reason() {
    let ctx = context(1);
    let handle = ctx.handle();
    let waiting = tokio::spawn({
        let handle = handle.clone();
        async move { handle.lost().await }
    });
    tokio::task::yield_now().await;
    handle.link_lost("end of file");
    handle.link_lost("later");
    let lost = tokio::time::timeout(Duration::from_secs(5), waiting).await.expect("the lost link was not seen").unwrap();
    assert_eq!(lost.reason(), "end of file");
    assert!(handle.is_lost());
}

/// A blocking call that overruns the operation's deadline is abandoned: a `LinkError` at the
/// deadline, not when the call ends, and the link reported lost.
#[tokio::test]
async fn a_blocking_call_past_its_deadline_is_abandoned_and_the_link_lost() {
    let ctx = context(1);
    let handle = ctx.handle();
    let (release, held) = std::sync::mpsc::channel::<()>();
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        under_deadline(
            Instant::now() + Duration::from_millis(100),
            ctx.blocking(move || {
                let _ = held.recv();
            }),
        ),
    )
    .await
    .expect("the blocking call was not bounded by its deadline");
    assert!(result.is_err(), "an overrun was not a LinkError");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(handle.is_lost(), "the overrun did not lose the link");
    drop(release);
}

/// At most `ABANDONED_CAP` abandoned calls may still be running; past it a blocking call is
/// refused at once — and accepted again once one of them ends.
#[tokio::test]
async fn abandoned_blocking_calls_are_capped() {
    let abandoned = Arc::new(Abandoned::default());
    let mut releases = Vec::new();
    for generation in 0..ABANDONED_CAP as u64 {
        let ctx = LinkContext::new(generation, "i", Reports::new(), abandoned.clone(), Arc::new(OpClock::new()));
        let (release, held) = std::sync::mpsc::channel::<()>();
        releases.push(release);
        let call = ctx.blocking(move || {
            let _ = held.recv();
        });
        assert!(under_deadline(Instant::now() + Duration::from_millis(20), call).await.is_err());
    }
    let ctx = LinkContext::new(99, "i", Reports::new(), abandoned.clone(), Arc::new(OpClock::new()));
    let started = Instant::now();
    assert!(
        under_deadline(Instant::now() + Duration::from_secs(5), ctx.blocking(|| 1)).await.is_err(),
        "a blocking call past the cap was run"
    );
    assert!(started.elapsed() < Duration::from_secs(1), "the refusal waited");
    drop(releases.pop());
    let deadline = Instant::now() + Duration::from_secs(5);
    while abandoned.running.load(Ordering::SeqCst) >= ABANDONED_CAP {
        assert!(Instant::now() < deadline, "an abandoned call that ended was never uncounted");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let ctx = LinkContext::new(100, "i", Reports::new(), abandoned, Arc::new(OpClock::new()));
    assert_eq!(under_deadline(Instant::now() + Duration::from_secs(5), ctx.blocking(|| 7)).await, Ok(7));
}

/// A blocking call whose future is dropped — the operation that awaited it cancelled, by its link's
/// loss or its own deadline — while its thread still runs stays counted until the thread ends
/// (review C3: only a deadline's own branch counted it, so cancellations leaked uncounted threads).
#[tokio::test]
async fn a_blocking_call_whose_future_is_dropped_stays_counted_until_its_thread_ends() {
    let abandoned = Arc::new(Abandoned::default());
    let ctx = LinkContext::new(1, "i", Reports::new(), abandoned.clone(), Arc::new(OpClock::new()));
    let (release, held) = std::sync::mpsc::channel::<()>();
    let dropped = tokio::time::timeout(
        Duration::from_millis(50),
        under_deadline(
            Instant::now() + Duration::from_secs(10),
            ctx.blocking(move || {
                let _ = held.recv();
            }),
        ),
    )
    .await;
    assert!(dropped.is_err(), "the call ended on its own, so this test proves nothing");
    assert_eq!(abandoned.running.load(Ordering::SeqCst), 1, "a dropped call whose thread still runs was not counted");
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(5);
    while abandoned.running.load(Ordering::SeqCst) > 0 {
        assert!(Instant::now() < deadline, "a call whose thread ended was never uncounted");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// A helper's blocking calls — no operation's deadline over them — count too: past the cap of
/// threads still running, a call is refused at once (review MINOR 11: a reader on a synchronous
/// serial library leaked an uncounted thread per link).
#[tokio::test(flavor = "multi_thread")]
async fn a_helpers_blocking_calls_count_towards_the_cap() {
    let abandoned = Arc::new(Abandoned::default());
    let ctx = LinkContext::new(1, "i", Reports::new(), abandoned.clone(), Arc::new(OpClock::new()));
    let handle = ctx.handle();
    let mut releases = Vec::new();
    let mut calls = Vec::new();
    for _ in 0..HELPER_CAP {
        let (release, held) = std::sync::mpsc::channel::<()>();
        releases.push(release);
        let handle = handle.clone();
        calls.push(tokio::spawn(async move {
            handle
                .blocking(move || {
                    let _ = held.recv();
                })
                .await
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    while abandoned.helpers.load(Ordering::SeqCst) < HELPER_CAP {
        assert!(Instant::now() < deadline, "the helper's calls were never counted: {}", abandoned.helpers.load(Ordering::SeqCst));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = Instant::now();
    let past_cap = tokio::time::timeout(Duration::from_secs(2), handle.blocking(|| 1)).await;
    assert!(matches!(past_cap, Ok(Err(_))), "a blocking call past the cap was run: {past_cap:?}");
    assert!(started.elapsed() < Duration::from_secs(1), "the refusal waited");
    drop(releases);
    for call in calls {
        let _ = call.await;
    }
}

/// Dropping the context — the link lost — cancels its helpers, those started through a handle too.
/// Helpers run on the worker's own runtime, so the tests run them in a `LocalSet`, as the worker
/// does.
#[tokio::test]
async fn dropping_the_context_cancels_its_helpers() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut ctx = context(1);
            let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
            let through_handle = tx.clone();
            ctx.spawn(async move {
                let _keep = tx;
                std::future::pending::<()>().await;
            });
            ctx.handle().spawn(async move {
                let _keep = through_handle;
                std::future::pending::<()>().await;
            });
            tokio::task::yield_now().await;
            drop(ctx);
            let closed = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("a helper outlived its link");
            assert!(closed.is_none());
        })
        .await;
}

/// A helper need not be `Send`: it runs on the device worker's own single-threaded runtime, as the
/// driver does (review MINOR 12).
#[tokio::test]
async fn a_helper_need_not_be_send() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut ctx = context(1);
            let handle = ctx.handle();
            let ran = std::rc::Rc::new(std::cell::Cell::new(0));
            let (first, second) = (ran.clone(), ran.clone());
            ctx.spawn(async move { first.set(first.get() + 1) });
            handle.spawn(async move { second.set(second.get() + 1) });
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert_eq!(ran.get(), 2, "the helpers did not run");
            assert!(handle.is_lost(), "a helper that returned did not lose the link");
        })
        .await;
}

/// A handle used to start a helper off the device worker's thread starts none: helpers run on the
/// worker's runtime only (a driver bug, ERROR `driver_contract_violation`).
#[tokio::test]
async fn a_helper_started_off_the_worker_thread_is_not_started() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let ctx = context(1);
            let handle = ctx.handle();
            let started = std::sync::Arc::new(AtomicBool::new(false));
            let flag = started.clone();
            let elsewhere = std::thread::spawn(move || handle.spawn(async move { flag.store(true, Ordering::SeqCst) }));
            assert!(elsewhere.join().is_ok(), "starting a helper off the worker thread panicked");
            tokio::time::sleep(Duration::from_millis(50)).await;
            assert!(!started.load(Ordering::SeqCst), "a helper started off the worker thread ran");
            drop(ctx);
        })
        .await;
}

/// An event without a string `kind` is a driver bug: dropped, with an ERROR.
#[test]
fn an_event_without_a_kind_is_dropped() {
    let log = crate::test_support::Capture::start();
    let reports = Reports::new();
    let reporter = Reporter::new(reports.clone());
    reporter.event(serde_json::json!({"target": "tv"}));
    reporter.event(serde_json::json!({"kind": "code", "code": "x"}));
    assert_eq!(reports.take().events.len(), 1);
    assert_eq!(log.of_kind("driver_contract_violation").len(), 1);
}

/// The cap is reserved atomically: of many calls racing for its places, exactly the cap get one —
/// never more (the R1 re-gate: no control saw a check-then-add, which lets a burst past the cap).
#[test]
fn racing_calls_never_reserve_past_the_cap() {
    for _ in 0..50 {
        let abandoned = Arc::new(Abandoned::default());
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let racers: Vec<_> = (0..16)
            .map(|_| {
                let abandoned = abandoned.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    Slot::reserve(&abandoned, Kind::Operation)
                })
            })
            .collect();
        let slots: Vec<Slot> = racers.into_iter().filter_map(|r| r.join().expect("a racer panicked")).collect();
        assert!(slots.len() <= ABANDONED_CAP, "{} racing calls reserved past the cap of {ABANDONED_CAP}", slots.len());
        assert_eq!(slots.len(), ABANDONED_CAP, "racing calls left the cap's places unused");
        drop(slots);
        assert_eq!(abandoned.running.load(Ordering::SeqCst), 0, "a released place stayed counted");
    }
}

/// A transient helper — a delayed re-query, a debounce timer — may end: its return is not a lost
/// link, as a link-critical helper's is; its panic is still a driver bug and loses the link (the R1
/// re-gate's R17: every helper that returned lost the link).
#[tokio::test]
async fn a_transient_helper_that_returns_keeps_the_link() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let mut ctx = context(1);
            let handle = ctx.handle();
            ctx.spawn_transient(async {});
            handle.spawn_transient(async {});
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert!(!handle.is_lost(), "a transient helper that returned lost the link");
            handle.spawn_transient(async { panic!("a transient helper's bug") });
            for _ in 0..10 {
                tokio::task::yield_now().await;
            }
            assert!(handle.is_lost(), "a transient helper's panic kept the link");
        })
        .await;
}

/// Long-lived helper reads — four ports, a reader per zone — do not fill the operations' cap: a
/// helper's blocking calls have a budget of their own, so a driver's operation still gets its
/// thread, and no cap stays "full" to end the process (the re-gate 3 S7).
#[tokio::test(flavor = "multi_thread")]
async fn four_long_helper_reads_leave_the_operations_cap_free() {
    let abandoned = Arc::new(Abandoned::default());
    let ctx = LinkContext::new(1, "i", Reports::new(), abandoned.clone(), Arc::new(OpClock::new()));
    let handle = ctx.handle();
    let mut releases = Vec::new();
    let mut readers = Vec::new();
    for _ in 0..ABANDONED_CAP {
        let (release, held) = std::sync::mpsc::channel::<()>();
        releases.push(release);
        let handle = handle.clone();
        readers.push(tokio::spawn(async move {
            handle
                .blocking(move || {
                    let _ = held.recv();
                })
                .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let operation = under_deadline(Instant::now() + Duration::from_secs(2), ctx.blocking(|| 7)).await;
    assert_eq!(operation, Ok(7), "four helper reads filled the operations' cap");
    assert!(abandoned.full_for().is_none(), "four helper reads made the cap full");
    drop(releases);
    for reader in readers {
        let _ = reader.await;
    }
}

/// A refusal stamped late — just after a place was freed — does not leave the cap "full" once a
/// call has its place again (the re-gate 3 S6: a stale stamp could end the process as stuck).
#[tokio::test(flavor = "multi_thread")]
async fn a_late_refusal_stamp_is_cleared_by_the_next_call_that_gets_its_place() {
    let abandoned = Arc::new(Abandoned::default());
    let ctx = LinkContext::new(1, "i", Reports::new(), abandoned.clone(), Arc::new(OpClock::new()));
    // The stamp of a refusal that lost the race with a place's release.
    abandoned.refused();
    assert!(abandoned.full_for().is_some());
    let held = under_deadline(Instant::now() + Duration::from_secs(2), ctx.blocking(|| std::thread::sleep(Duration::from_millis(300))));
    let checking = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        abandoned.full_for()
    };
    let (_, during) = tokio::join!(held, checking);
    assert!(during.is_none(), "a stale refusal stamp stood while calls had their places");
}

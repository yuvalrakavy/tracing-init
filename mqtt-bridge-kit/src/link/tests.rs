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
        ctx.blocking(move || {
            let _ = held.recv();
        }),
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
    for _ in 0..ABANDONED_CAP {
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
    while abandoned.running.load(Ordering::SeqCst) < ABANDONED_CAP {
        assert!(Instant::now() < deadline, "the helper's calls were never counted: {}", abandoned.running.load(Ordering::SeqCst));
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

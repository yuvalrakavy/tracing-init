//! The link context: a lost link said once, a blocking call bounded and its abandoned threads
//! capped.

use std::time::Duration;

use super::*;

fn context(generation: u64) -> LinkContext {
    LinkContext::new(generation, "i", Reports::new(), Arc::new(Abandoned::default()))
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
    handle.set_deadline(Some(Instant::now() + Duration::from_millis(100)));
    let (release, held) = std::sync::mpsc::channel::<()>();
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        ctx.blocking(move || {
            let _ = held.recv();
        }),
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
        let ctx = LinkContext::new(generation, "i", Reports::new(), abandoned.clone());
        ctx.handle().set_deadline(Some(Instant::now() + Duration::from_millis(20)));
        let (release, held) = std::sync::mpsc::channel::<()>();
        releases.push(release);
        assert!(ctx
            .blocking(move || {
                let _ = held.recv();
            })
            .await
            .is_err());
    }
    let ctx = LinkContext::new(99, "i", Reports::new(), abandoned.clone());
    ctx.handle().set_deadline(Some(Instant::now() + Duration::from_secs(5)));
    let started = Instant::now();
    assert!(ctx.blocking(|| 1).await.is_err(), "a blocking call past the cap was run");
    assert!(started.elapsed() < Duration::from_secs(1), "the refusal waited");
    drop(releases.pop());
    let deadline = Instant::now() + Duration::from_secs(5);
    while abandoned.running.load(Ordering::SeqCst) >= ABANDONED_CAP {
        assert!(Instant::now() < deadline, "an abandoned call that ended was never uncounted");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let ctx = LinkContext::new(100, "i", Reports::new(), abandoned);
    ctx.handle().set_deadline(Some(Instant::now() + Duration::from_secs(5)));
    assert_eq!(ctx.blocking(|| 7).await, Ok(7));
}

/// Dropping the context — the link lost — cancels its helpers.
#[tokio::test]
async fn dropping_the_context_cancels_its_helpers() {
    let mut ctx = context(1);
    let (tx, mut rx) = tokio::sync::mpsc::channel::<()>(1);
    ctx.spawn(async move {
        let _keep = tx;
        std::future::pending::<()>().await;
    });
    drop(ctx);
    let closed = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("the helper outlived its link");
    assert!(closed.is_none());
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

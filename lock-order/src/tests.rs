//! Each test uses its own class names: the order graph and the recorded cycles are global, and
//! tests run in parallel.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::{branch, cycles, holding, long_waits, scope, set_report_after, sync, tokio_sync, waits_on};

/// How long a test waits on another thread before it fails instead of hanging the suite.
const BOUND: Duration = Duration::from_secs(10);

/// Join `h`, or fail the test if it has not finished within [`BOUND`].
fn join_within<T>(h: std::thread::JoinHandle<T>, what: &str) -> std::thread::Result<T> {
    let deadline = Instant::now() + BOUND;
    while !h.is_finished() {
        assert!(Instant::now() < deadline, "{what} did not finish within {BOUND:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
    h.join()
}

fn cycle_with(a: &str, b: &str) -> Option<String> {
    cycles().into_iter().find(|c| c.contains(&format!("`{a}`")) && c.contains(&format!("`{b}`")))
}

fn any_cycle_with(class: &str) -> Option<String> {
    cycles().into_iter().find(|c| c.contains(&format!("`{class}`")))
}

#[test]
fn two_std_locks_taken_in_both_orders_on_two_threads_are_a_cycle_without_deadlocking() {
    let a = Arc::new(sync::Mutex::new("std-inv-a", ()));
    let b = Arc::new(sync::Mutex::new("std-inv-b", ()));
    let (a1, b1) = (a.clone(), b.clone());
    let first = std::thread::spawn(move || {
        let _ga = a1.lock().unwrap();
        let _gb = b1.lock().unwrap();
    });
    join_within(first, "the first thread").unwrap();
    assert!(cycle_with("std-inv-a", "std-inv-b").is_none(), "one order alone is no cycle");
    let second = std::thread::spawn(move || {
        let _gb = b.lock().unwrap();
        let _ga = a.lock().unwrap();
    });
    join_within(second, "the second thread").unwrap();
    let c = cycle_with("std-inv-a", "std-inv-b").expect("both orders are a cycle, though nothing deadlocked");
    assert!(c.contains("tests.rs"), "each edge names where it was taken: {c}");
}

#[tokio::test(flavor = "multi_thread")]
async fn tokio_locks_held_across_an_await_in_one_task_and_inverted_in_another_are_a_cycle() {
    let a = Arc::new(tokio_sync::Mutex::new("tok-inv-a", ()));
    let b = Arc::new(tokio_sync::Mutex::new("tok-inv-b", ()));
    let (a1, b1) = (a.clone(), b.clone());
    tokio::spawn(async move {
        let _ga = a1.lock().await;
        tokio::task::yield_now().await;
        let _gb = b1.lock().await;
    })
    .await
    .unwrap();
    tokio::spawn(async move {
        let _gb = b.lock().await;
        let _ga = a.lock().await;
    })
    .await
    .unwrap();
    assert!(cycle_with("tok-inv-a", "tok-inv-b").is_some(), "{:?}", cycles());
}

#[tokio::test]
async fn the_same_instance_again_is_reported_and_two_instances_of_one_class_are_not() {
    let l = tokio_sync::RwLock::new("again", ());
    let _first = l.read().await;
    let _second = l.read().await; // no writer queued, so this does not block — but it can.
    assert!(any_cycle_with("again").is_some_and(|c| c.contains("waits on itself")), "{:?}", cycles());

    let x = tokio_sync::Mutex::new("two-instances", ());
    let y = tokio_sync::Mutex::new("two-instances", ());
    let _gx = x.lock().await;
    let _gy = y.lock().await;
    assert!(any_cycle_with("two-instances").is_none(), "{:?}", cycles());
}

#[tokio::test(flavor = "multi_thread")]
async fn waiting_on_a_consumer_while_holding_a_lock_it_takes_is_a_cycle() {
    let store = Arc::new(tokio_sync::RwLock::new("cons-store", ()));
    // The consumer takes the store lock while it handles a message.
    let s1 = store.clone();
    tokio::spawn(holding("cons-processor", async move {
        let _w = s1.write().await;
    }))
    .await
    .unwrap();
    // A caller holding the store lock waits on the consumer: 07-07's shape.
    let _r = store.read().await;
    let _waiting = waits_on("cons-processor");
    assert!(cycle_with("cons-store", "cons-processor").is_some(), "{:?}", cycles());
}

#[tokio::test]
async fn a_branch_is_its_own_context() {
    let a = tokio_sync::Mutex::new("br-a", ());
    let b = tokio_sync::Mutex::new("br-b", ());
    let holds_a = branch(async {
        let _ga = a.lock().await;
        tokio::task::yield_now().await;
    });
    let takes_b = branch(async {
        let _gb = b.lock().await;
    });
    tokio::join!(holds_a, takes_b);
    // Inverted in one context: only a b → a edge exists, so no cycle.
    let _gb = b.lock().await;
    let _ga = a.lock().await;
    assert!(any_cycle_with("br-a").is_none(), "a guard one branch held was read as the other's: {:?}", cycles());
}

/// One thread, two tasks: the context is the task, so a guard one task holds across an `.await`
/// is not read as held by the task the thread runs meanwhile.
#[tokio::test(flavor = "current_thread")]
async fn two_tasks_on_one_thread_are_two_contexts() {
    let a = Arc::new(tokio_sync::Mutex::new("tasks-a", ()));
    let b = Arc::new(tokio_sync::Mutex::new("tasks-b", ()));
    let (gate_tx, gate_rx) = tokio::sync::oneshot::channel::<()>();
    let a1 = a.clone();
    let holder = tokio::spawn(async move {
        let _ga = a1.lock().await;
        let _ = gate_rx.await; // the other task runs on this thread meanwhile
    });
    tokio::task::yield_now().await;
    let b1 = b.clone();
    tokio::spawn(async move {
        let _gb = b1.lock().await;
    })
    .await
    .unwrap();
    let _ = gate_tx.send(());
    holder.await.unwrap();
    let _gb = b.lock().await;
    let _ga = a.lock().await;
    assert!(any_cycle_with("tasks-a").is_none(), "another task's guard was read as held: {:?}", cycles());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guard_dropped_on_another_thread_is_released_and_an_adopted_one_is_held_there() {
    let a = tokio_sync::RwLock::new("moved-a", ());
    let b = tokio_sync::Mutex::new("moved-b", ());
    let guard = a.read_owned().await;
    join_within(std::thread::spawn(move || drop(guard)), "the dropping thread").unwrap();
    let _gb = b.lock().await; // a was released: no a → b edge.
    drop(_gb);
    let _gb = b.lock().await;
    let _ga = a.read().await;
    assert!(any_cycle_with("moved-a").is_none(), "a released guard still read as held: {:?}", cycles());

    let c = tokio_sync::RwLock::new("adopt-c", ());
    let d = Arc::new(sync::Mutex::new("adopt-d", ()));
    let mut guard = c.read_owned().await;
    let d1 = d.clone();
    tokio::task::spawn_blocking(move || {
        guard.adopt();
        let _gd = d1.lock().unwrap(); // c is held here now: c → d.
        drop(guard);
    })
    .await
    .unwrap();
    let _gd = d.lock().unwrap();
    let _gc = c.try_read().unwrap();
    drop(_gc);
    drop(_gd);
    let gd = d.lock().unwrap();
    let blocking_c = c.read();
    let held = tokio::time::timeout(Duration::from_secs(5), blocking_c).await.expect("c is free");
    drop(held);
    drop(gd);
    assert!(cycle_with("adopt-c", "adopt-d").is_some(), "the adopted guard's edge was missed: {:?}", cycles());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lock_taken_through_block_in_place_and_block_on_is_seen_under_the_task_s_locks() {
    let a = tokio_sync::Mutex::new("bridge-a", ());
    let b = Arc::new(tokio_sync::Mutex::new("bridge-b", ()));
    let _ga = a.lock().await;
    let b1 = b.clone();
    let handle = tokio::runtime::Handle::current();
    tokio::task::block_in_place(|| {
        handle.block_on(async {
            let _gb = b1.lock().await;
        })
    });
    drop(_ga);
    let _gb = b.lock().await;
    let _ga = a.lock().await;
    assert!(cycle_with("bridge-a", "bridge-b").is_some(), "the bridge hid the held lock: {:?}", cycles());
}

#[test]
fn try_lock_records_no_edge() {
    let a = sync::Mutex::new("try-a", ());
    let b = sync::Mutex::new("try-b", ());
    {
        let _ga = a.lock().unwrap();
        let _gb = b.try_lock().unwrap();
    }
    let _gb = b.lock().unwrap();
    let _ga = a.lock().unwrap();
    assert!(any_cycle_with("try-a").is_none(), "{:?}", cycles());
}

#[test]
fn a_scope_is_a_pseudo_lock() {
    let a = sync::Mutex::new("scope-a", ());
    {
        let _ga = a.lock().unwrap();
        scope("scope-txn", || ());
    }
    scope("scope-txn", || {
        let _ga = a.lock().unwrap();
    });
    assert!(cycle_with("scope-a", "scope-txn").is_some(), "{:?}", cycles());
}

#[test]
fn a_consistent_order_a_thousand_times_is_no_cycle() {
    let a = sync::Mutex::new("steady-a", ());
    let b = sync::Mutex::new("steady-b", ());
    for _ in 0..1000 {
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap();
    }
    assert!(any_cycle_with("steady-a").is_none(), "{:?}", cycles());
}

/// The runtime's only worker blocks on a std lock; the watchdog thread reports the wait anyway,
/// naming where the lock is held.
#[test]
fn a_long_wait_is_reported_by_the_watchdog_thread_while_the_waiter_blocks() {
    set_report_after("long-std", Duration::from_millis(200));
    let l = Arc::new(sync::Mutex::new("long-std", ()));
    let (held_tx, held_rx) = mpsc::channel();
    let l1 = l.clone();
    let holder = std::thread::spawn(move || {
        let _g = l1.lock().unwrap();
        held_tx.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(700));
    });
    held_rx.recv_timeout(BOUND).expect("the holder took the lock");
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let _g = l.lock().unwrap(); // blocks the runtime's only worker for ~700 ms
    });
    join_within(holder, "the holder").unwrap();
    let seen: Vec<_> = long_waits().into_iter().filter(|w| w.class == "long-std").collect();
    assert_eq!(seen.len(), 1, "one report per wait: {seen:?}");
    assert!(seen[0].holders.iter().any(|h| h.contains("tests.rs")), "names the holder: {:?}", seen[0]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_contended_tokio_wait_is_reported_and_an_uncontended_one_is_not_registered() {
    set_report_after("long-tok", Duration::from_millis(200));
    let l = Arc::new(tokio_sync::Mutex::new("long-tok", ()));
    {
        let _quick = l.lock().await;
    }
    let held = l.lock().await;
    let l2 = l.clone();
    let waiter = tokio::spawn(async move {
        let _g = l2.lock().await;
    });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(long_waits().iter().filter(|w| w.class == "long-tok").count(), 1, "{:?}", long_waits());
    drop(held);
    tokio::time::timeout(Duration::from_secs(5), waiter).await.expect("the waiter gets the lock").unwrap();
}

#[tokio::test]
async fn a_branch_inherits_what_is_held_around_it() {
    let a = tokio_sync::Mutex::new("inherit-a", ());
    let b = tokio_sync::Mutex::new("inherit-b", ());
    {
        let _ga = a.lock().await;
        branch(async {
            let _gb = b.lock().await; // a is held around this branch: a → b
        })
        .await;
    }
    let _gb = b.lock().await;
    let _ga = a.lock().await;
    assert!(cycle_with("inherit-a", "inherit-b").is_some(), "the branch lost the lock held around it: {:?}", cycles());
}

#[tokio::test]
async fn a_consumer_waiting_on_itself_is_reported_whatever_its_name_s_spelling() {
    let one: &'static str = Box::leak(String::from("self-consumer").into_boxed_str());
    let two: &'static str = Box::leak(String::from("self-consumer").into_boxed_str());
    holding(one, async move {
        let _waiting = waits_on(two);
    })
    .await;
    assert!(any_cycle_with("self-consumer").is_some_and(|c| c.contains("waits on itself")), "{:?}", cycles());
}

#[tokio::test]
async fn one_store_s_consumer_waiting_on_another_s_is_not_a_self_wait() {
    crate::holding_in("inst-consumer", 1, async {
        let _waiting = crate::waits_on_in("inst-consumer", 2);
    })
    .await;
    assert!(any_cycle_with("inst-consumer").is_none(), "{:?}", cycles());
    crate::holding_in("inst-consumer", 3, async {
        let _waiting = crate::waits_on_in("inst-consumer", 3);
    })
    .await;
    assert!(any_cycle_with("inst-consumer").is_some_and(|c| c.contains("waits on itself")), "{:?}", cycles());
}

#[tokio::test]
async fn a_guard_returned_out_of_a_branch_is_held_by_its_parent() {
    let a = Arc::new(tokio_sync::Mutex::new("ret-a", ()));
    let b = tokio_sync::Mutex::new("ret-b", ());
    let a1 = a.clone();
    let ga = branch(async move { a1.lock_owned().await }).await;
    let _gb = b.lock().await; // a is held here, returned by the branch: a → b
    drop(_gb);
    drop(ga);
    let _gb = b.lock().await;
    let _ga = a.lock().await;
    assert!(cycle_with("ret-a", "ret-b").is_some(), "the returned guard was lost with its branch: {:?}", cycles());
}

/// A guard returned out of a branch moves to its parent when the branch ends — and its drop must
/// release it there. Leaked, the parent "holds" the lock forever, and taking it again reads as a
/// false self-wait (review finding, 2026-10-02).
#[tokio::test]
async fn a_guard_returned_out_of_a_branch_releases_when_dropped() {
    let a = Arc::new(tokio_sync::Mutex::new("relret-a", ()));
    let a1 = a.clone();
    let g = branch(async move { a1.lock_owned().await }).await;
    drop(g);
    let _again = a.lock().await;
    assert!(any_cycle_with("relret-a").is_none(), "the dropped guard's record leaked into the parent: {:?}", cycles());
}

/// A branch cancelled while it holds a guard: the guard drops with the branch's future, and
/// must not be left behind in the parent.
#[tokio::test]
async fn a_cancelled_branch_releases_what_it_held() {
    let a = Arc::new(tokio_sync::Mutex::new("cancel-a", ()));
    let a1 = a.clone();
    let (taken_tx, taken_rx) = tokio::sync::oneshot::channel::<()>();
    let mut held = Box::pin(branch(async move {
        let _g = a1.lock_owned().await;
        let _ = taken_tx.send(());
        std::future::pending::<()>().await;
    }));
    tokio::select! {
        _ = &mut held => unreachable!("the branch never completes"),
        _ = taken_rx => {}
    }
    drop(held);
    let _again = a.lock().await;
    assert!(any_cycle_with("cancel-a").is_none(), "the cancelled branch's guard was left held: {:?}", cycles());
}

/// An owned guard returned out of a branch, then adopted by another task: the adoption must find
/// it where the branch's end moved it.
#[tokio::test(flavor = "multi_thread")]
async fn a_guard_from_an_ended_branch_can_be_adopted() {
    let a = Arc::new(tokio_sync::Mutex::new("adopt-ret-a", ()));
    let a1 = a.clone();
    let g = branch(async move { a1.lock_owned().await }).await;
    tokio::spawn(async move {
        let mut g = g;
        g.adopt();
        drop(g);
    })
    .await
    .unwrap();
    let _again = a.lock().await;
    assert!(any_cycle_with("adopt-ret-a").is_none(), "the adopted guard's record stayed in the parent: {:?}", cycles());
}

#[test]
fn a_self_wait_is_reported_once_per_site() {
    let l = sync::RwLock::new("self-once", ());
    let _first = l.read().unwrap();
    for _ in 0..1000 {
        let _again = l.try_read().unwrap(); // try: no wait, no report
    }
    let before = cycles().iter().filter(|c| c.contains("`self-once`")).count();
    assert_eq!(before, 0);
    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    let t = tokio_sync::RwLock::new("self-once-tok", ());
    rt.block_on(async {
        let _first = t.read().await;
        for _ in 0..1000 {
            let _again = t.read().await;
        }
    });
    assert_eq!(cycles().iter().filter(|c| c.contains("`self-once-tok`")).count(), 1, "{:?}", cycles());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_semaphore_permit_is_a_pseudo_lock_while_it_lives() {
    let sem = Arc::new(tokio::sync::Semaphore::new(1));
    let a = tokio_sync::Mutex::new("sem-a", ());
    {
        let (permit, _held) = crate::acquire("sem-window", sem.clone().acquire_owned()).await;
        let _ga = a.lock().await; // sem-window → sem-a
        drop(permit);
    }
    let _ga = a.lock().await;
    let (_permit, _held) = crate::acquire("sem-window", sem.clone().acquire_owned()).await;
    assert!(cycle_with("sem-window", "sem-a").is_some(), "{:?}", cycles());
}

// `sync::Condvar` (Store no-hang §14.2). A notifier on another thread proves nothing about the
// waiter's own record — holdings are per thread — so these look at the record itself. Every wait
// in them is bounded, so a regression fails the test instead of hanging the suite (review finding
// C-13).

/// Lock `m` from this thread until `state` reads `want`, the waiter's signal that it is inside
/// its wait (it set the state under the lock, and released the lock only by waiting).
fn until_state(m: &sync::Mutex<u8>, want: u8) -> sync::MutexGuard<'_, u8> {
    let deadline = Instant::now() + BOUND;
    loop {
        if let Ok(g) = m.try_lock() {
            if *g == want {
                return g;
            }
        }
        assert!(Instant::now() < deadline, "the waiter never reached its wait");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_condvar_wait_gives_up_its_class_and_takes_it_back() {
    let pair = Arc::new((sync::Mutex::new("cv-own", 0u8), sync::Condvar::new()));
    let instance = pair.0.instance();
    let (woke_tx, woke_rx) = mpsc::channel();
    let (looked_tx, looked_rx) = mpsc::channel::<()>();
    let p = pair.clone();
    let waiter = std::thread::spawn(move || {
        let (m, cv) = &*p;
        let mut g = m.lock().unwrap();
        *g = 1;
        while *g != 2 {
            g = cv.wait(g).unwrap();
        }
        woke_tx.send(()).unwrap();
        // Hold the re-taken guard while the test looks (or until it gives up).
        let _ = looked_rx.recv_timeout(BOUND);
    });
    let mut g = until_state(&pair.0, 1);
    // This thread holds the mutex now; the waiter, inside its wait, must not be recorded too.
    assert_eq!(crate::checker::holders_of(instance).len(), 1, "the waiter kept its record through the wait");
    *g = 2;
    pair.1.notify_all();
    drop(g);
    woke_rx.recv_timeout(BOUND).expect("the waiter woke");
    assert_eq!(crate::checker::holders_of(instance).len(), 1, "the waiter did not take its record back on wake");
    looked_tx.send(()).unwrap();
    join_within(waiter, "the waiter").unwrap();
}

#[test]
fn a_condvar_wait_holding_a_later_lock_reports_the_reacquisitions_cycle() {
    let a = sync::Mutex::new("cv-ord-a", ());
    let b = sync::Mutex::new("cv-ord-b", ());
    let cv = sync::Condvar::new();
    {
        let _ga = a.lock().unwrap();
        let _gb = b.lock().unwrap(); // a → b
    }
    assert!(cycle_with("cv-ord-a", "cv-ord-b").is_none(), "one order alone is no cycle");
    let ga = a.lock().unwrap();
    let gb = b.lock().unwrap();
    // Waiting on `a` while holding `b`: the wake re-takes `a` under `b`. std re-takes it before
    // returning, so the check must have happened before the wait.
    let (ga, _) = cv.wait_timeout(ga, Duration::from_millis(5)).unwrap();
    drop(gb);
    drop(ga);
    assert!(cycle_with("cv-ord-a", "cv-ord-b").is_some(), "the re-acquisition's order was not checked before the wait: {:?}", cycles());
}

/// A genuinely blocked re-acquisition is caught by the pre-wait check (Store no-hang §14.5; review
/// finding C-M3 / X-8). The waiter holds `a` and `b` and waits on `a`, so its wake re-takes `a`
/// under `b`; this thread takes `a`, notifies, and keeps `a` — the waiter is woken into a
/// re-acquisition it cannot complete while `a` is held here. Its cycle must already be on record:
/// a check made when `wait` returns has not run yet, and in a real deadlock never would.
#[test]
fn a_blocked_reacquisition_is_on_record_before_the_wait_returns() {
    let locks = Arc::new((sync::Mutex::new("cv-blk-a", 0u8), sync::Mutex::new("cv-blk-b", ()), sync::Condvar::new()));
    {
        let _ga = locks.0.lock().unwrap();
        let _gb = locks.1.lock().unwrap(); // a → b
    }
    assert!(cycle_with("cv-blk-a", "cv-blk-b").is_none(), "one order alone is no cycle");
    let (done_tx, done_rx) = mpsc::channel();
    let l = locks.clone();
    let waiter = std::thread::spawn(move || {
        let (a, b, cv) = &*l;
        let mut ga = a.lock().unwrap();
        let gb = b.lock().unwrap();
        *ga = 1;
        while *ga != 2 {
            ga = cv.wait(ga).unwrap();
        }
        drop(gb);
        drop(ga);
        done_tx.send(()).unwrap();
    });
    let mut ga = until_state(&locks.0, 1); // the waiter is inside its wait
    *ga = 2;
    locks.2.notify_all();
    // Woken, the waiter now blocks re-taking `a`, which this thread holds.
    std::thread::sleep(Duration::from_millis(50));
    let on_record = cycle_with("cv-blk-a", "cv-blk-b");
    drop(ga);
    done_rx.recv_timeout(BOUND).expect("the waiter finishes once `a` is free");
    join_within(waiter, "the waiter").unwrap();
    assert!(on_record.is_some(), "the re-acquisition's order was not checked before the wait: {:?}", cycles());
}

#[test]
fn a_poisoned_wake_takes_the_class_back_too() {
    let pair = Arc::new((sync::Mutex::new("cv-poison", 0u8), sync::Condvar::new()));
    let instance = pair.0.instance();
    let p = pair.clone();
    let waiter = std::thread::spawn(move || {
        let (m, cv) = &*p;
        let mut g = m.lock().unwrap();
        *g = 1;
        let g = match cv.wait_while(g, |s| *s != 2) {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let held = crate::checker::holders_of(instance).len();
        drop(g);
        held
    });
    drop(until_state(&pair.0, 1));
    let p = pair.clone();
    let poisoner = std::thread::spawn(move || {
        let mut g = p.0.lock().unwrap();
        *g = 2;
        p.1.notify_all();
        panic!("poison the mutex under the waiter");
    });
    assert!(join_within(poisoner, "the poisoner").is_err());
    assert_eq!(join_within(waiter, "the waiter").unwrap(), 1, "a poisoned wake left the waiter without its record");
}

#[test]
fn a_condvar_wait_timeout_returns_at_its_bound() {
    let m = sync::Mutex::new("cv-timeout", ());
    let cv = sync::Condvar::new();
    let started = Instant::now();
    let (g, timed_out) = cv.wait_timeout(m.lock().unwrap(), Duration::from_millis(20)).unwrap();
    assert!(timed_out.timed_out());
    assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    drop(g);
    assert!(any_cycle_with("cv-timeout").is_none(), "{:?}", cycles());
}

/// `wait_while`'s predicate runs under the mutex, so it runs holding the guard's record: a lock it
/// takes orders after the condvar's mutex, like any lock taken under it, and re-taking the mutex
/// there is a self-wait the checker can see (review finding X-4 / C-11). Before the wait and after
/// the wake both.
#[test]
fn a_wait_while_predicate_runs_holding_the_condvar_s_mutex() {
    let locks = Arc::new((sync::Mutex::new("cv-pred-m", 0u8), sync::Mutex::new("cv-pred-b", ()), sync::Condvar::new()));
    let instance = locks.0.instance();
    let l = locks.clone();
    let waiter = std::thread::spawn(move || {
        let (m, b, cv) = &*l;
        let mut g = m.lock().unwrap();
        *g = 1;
        let mut held = Vec::new();
        let g = cv
            .wait_while(g, |s| {
                held.push(crate::checker::holds_now(instance));
                let _gb = b.lock().unwrap();
                *s != 2
            })
            .unwrap();
        drop(g);
        held
    });
    let mut g = until_state(&locks.0, 1);
    *g = 2;
    locks.2.notify_all();
    drop(g);
    let held = join_within(waiter, "the waiter").unwrap();
    assert!(
        held.len() >= 2 && held.iter().all(|h| *h),
        "the predicate ran without the guard's record (before the wait, after the wake): {held:?}"
    );
    {
        let _gb = locks.1.lock().unwrap();
        let _gm = locks.0.lock().unwrap(); // b → m, against the m → b the predicate took
    }
    assert!(
        cycle_with("cv-pred-m", "cv-pred-b").is_some(),
        "a lock taken in the predicate was not ordered after the condvar's mutex: {:?}",
        cycles()
    );
}

/// The same for `wait_timeout_while`, whose predicate stops the wait after one timed-out round.
#[test]
fn a_wait_timeout_while_predicate_runs_holding_the_condvar_s_mutex() {
    let m = sync::Mutex::new("cv-tpred-m", ());
    let b = sync::Mutex::new("cv-tpred-b", ());
    let cv = sync::Condvar::new();
    let mut held = Vec::new();
    let (g, result) = cv
        .wait_timeout_while(m.lock().unwrap(), Duration::from_millis(20), |_| {
            held.push(crate::checker::holds_now(m.instance()));
            let _gb = b.lock().unwrap();
            held.len() < 2
        })
        .unwrap();
    drop(g);
    assert!(!result.timed_out(), "the predicate ended the wait, not the bound");
    assert!(held.len() >= 2 && held.iter().all(|h| *h), "the predicate ran without the guard's record: {held:?}");
    {
        let _gb = b.lock().unwrap();
        let _gm = m.lock().unwrap();
    }
    assert!(
        cycle_with("cv-tpred-m", "cv-tpred-b").is_some(),
        "a lock taken in the predicate was not ordered after the condvar's mutex: {:?}",
        cycles()
    );
}

/// `wait_timeout_while` bounds the whole wait however often it is woken: std's contract, kept now
/// that it loops over this wrapper's `wait_timeout`.
#[test]
fn a_wait_timeout_while_keeps_its_bound_across_wakes() {
    let shared = Arc::new((sync::Mutex::new("cv-bound", ()), sync::Condvar::new(), AtomicBool::new(false)));
    let s = shared.clone();
    let notifier = std::thread::spawn(move || {
        let until = Instant::now() + Duration::from_secs(5);
        while !s.2.load(Ordering::SeqCst) && Instant::now() < until {
            s.1.notify_all();
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let started = Instant::now();
    let (g, result) = shared.1.wait_timeout_while(shared.0.lock().unwrap(), Duration::from_millis(100), |_| true).unwrap();
    let took = started.elapsed();
    drop(g);
    shared.2.store(true, Ordering::SeqCst);
    join_within(notifier, "the notifier").unwrap();
    assert!(result.timed_out(), "a condition that never cleared reports its timeout");
    assert!(took < Duration::from_secs(2), "the bound restarted at each wake: the wait took {took:?}");
}

// A guard gives up its record before its lock (review finding C-12). The next holder can take the
// lock the moment it is free, and must not find the last holder's record still on file: a wedge
// report naming a holder that has let go, a holder count one too many (the poisoned-wake test's
// flake).

/// The holder's half: keep `guard` until the test says to drop it.
struct Holder {
    ctx: mpsc::Sender<crate::checker::Context>,
    go: mpsc::Receiver<()>,
    done: mpsc::Sender<()>,
}

impl Holder {
    fn hold<G>(self, guard: G) {
        self.ctx.send(crate::checker::current()).unwrap();
        let go = self.go.recv_timeout(BOUND);
        drop(guard);
        let _ = self.done.send(());
        go.expect("the test said when to drop the guard");
    }
}

/// `spawn` takes the lock on a thread of its own and hands the guard to [`Holder::hold`]. With the
/// holder's records frozen, the guard is dropped: its record cannot go meanwhile, so the lock must
/// not come free — the record goes first.
fn gives_up_its_record_before_its_lock(
    what: &str,
    instance: usize,
    raw_free: impl Fn() -> bool,
    spawn: impl FnOnce(Holder) -> std::thread::JoinHandle<()>,
) {
    let (ctx_tx, ctx_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel();
    let (done_tx, done_rx) = mpsc::channel();
    let holder = spawn(Holder { ctx: ctx_tx, go: go_rx, done: done_tx });
    let ctx = ctx_rx.recv_timeout(BOUND).unwrap_or_else(|_| panic!("{what}: the holder never took its lock"));
    assert!(!raw_free(), "{what}: the lock is taken");
    crate::checker::with_records_frozen(ctx, instance, |on_file| {
        assert!(on_file(), "{what}: the holder's record is on file");
        go_tx.send(()).unwrap();
        let until = Instant::now() + Duration::from_millis(200);
        while Instant::now() < until {
            assert!(!(raw_free() && on_file()), "{what}: the lock was free while its holder's record was still on file");
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    done_rx.recv_timeout(BOUND).unwrap_or_else(|_| panic!("{what}: the holder never finished dropping its guard"));
    join_within(holder, what).unwrap();
    assert!(raw_free(), "{what}: the lock is free once the guard is gone");
    assert!(crate::checker::holders_of(instance).is_empty(), "{what}: and so is its record");
}

#[test]
fn a_std_guard_gives_up_its_record_before_its_lock() {
    let m = Arc::new(sync::Mutex::new("drop-std-mutex", ()));
    let m1 = m.clone();
    gives_up_its_record_before_its_lock(
        "sync::MutexGuard",
        m.instance(),
        || m.raw().try_lock().is_ok(),
        move |h| std::thread::spawn(move || h.hold(m1.lock().unwrap())),
    );
    let l = Arc::new(sync::RwLock::new("drop-std-rwlock", ()));
    let l1 = l.clone();
    gives_up_its_record_before_its_lock(
        "sync::RwLockReadGuard",
        l.instance(),
        || l.raw().try_write().is_ok(),
        move |h| std::thread::spawn(move || h.hold(l1.read().unwrap())),
    );
    let l1 = l.clone();
    gives_up_its_record_before_its_lock(
        "sync::RwLockWriteGuard",
        l.instance(),
        || l.raw().try_write().is_ok(),
        move |h| std::thread::spawn(move || h.hold(l1.write().unwrap())),
    );
}

/// The tokio guards, taken through a root `block_on` (whose context is its thread) and dropped
/// outside it.
#[test]
fn a_tokio_guard_gives_up_its_record_before_its_lock() {
    fn on_a_runtime<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().build().unwrap().block_on(f)
    }
    let m = Arc::new(tokio_sync::Mutex::new("drop-tok-mutex", ()));
    for owned in [false, true] {
        let m1 = m.clone();
        gives_up_its_record_before_its_lock(
            if owned { "tokio_sync::OwnedMutexGuard" } else { "tokio_sync::MutexGuard" },
            m.instance(),
            || m.raw().try_lock().is_ok(),
            move |h| {
                std::thread::spawn(move || if owned { h.hold(on_a_runtime(m1.lock_owned())) } else { h.hold(on_a_runtime(m1.lock())) })
            },
        );
    }
    let l = Arc::new(tokio_sync::RwLock::new("drop-tok-rwlock", ()));
    for (what, which) in [
        ("tokio_sync::RwLockReadGuard", 0),
        ("tokio_sync::RwLockWriteGuard", 1),
        ("tokio_sync::OwnedRwLockReadGuard", 2),
        ("tokio_sync::OwnedRwLockWriteGuard", 3),
    ] {
        let l1 = l.clone();
        gives_up_its_record_before_its_lock(
            what,
            l.instance(),
            || l.raw().try_write().is_ok(),
            move |h| {
                std::thread::spawn(move || match which {
                    0 => h.hold(on_a_runtime(l1.read())),
                    1 => h.hold(on_a_runtime(l1.write())),
                    2 => h.hold(on_a_runtime(l1.read_owned())),
                    _ => h.hold(on_a_runtime(l1.write_owned())),
                })
            },
        );
    }
}

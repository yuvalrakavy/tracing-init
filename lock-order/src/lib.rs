//! Lock wrappers that record, per lock **class**, the order a running program waits on them in,
//! and report the first cycle — lockdep for `std` and `tokio` locks, and for the tasks a program
//! waits on (Store's no-hang spec, §13.9).
//!
//! Whether two locks can deadlock depends on the order a running context takes them in, which
//! syntax cannot see. So every lock is built with a class — the key of its row in the wait
//! registry — and each blocking acquisition, **before it waits**, records an edge from every class
//! its context already holds. An edge that closes a cycle is reported once: an ERROR
//! (`kind = "lock_order_cycle"`) naming each edge and where it was first taken, recorded for
//! [`cycles`]. A run that never deadlocks still reports, since the two orders need not meet in
//! time.
//!
//! ```
//! let cache = lock_order::sync::Mutex::new("template-cache", Vec::<u8>::new());
//! cache.lock().unwrap().push(1);
//! ```
//!
//! # Not only locks
//!
//! A task others wait on — a command processor, a timer manager — is a class too (lockdep's
//! workqueue annotation): it runs each message inside [`holding`], and whoever waits on it calls
//! [`waits_on`] first. A lock held while waiting on a task that takes that lock is then a cycle
//! like any other. A blocking primitive the wrappers do not cover runs inside [`scope`].
//!
//! # The context
//!
//! A tokio task (`tokio::task::try_id()`), else the thread. Futures a task polls concurrently —
//! `join!`, `join_all`, `select!` branches — share the task: wrap each that takes a lock in
//! [`branch`], so one branch's guard is not read as held by another. A guard releases in the
//! context that took it, wherever it is dropped; an owned guard handed to another task or thread
//! is `adopt`ed there.
//!
//! # The watchdog
//!
//! An order no run took can still deadlock in production. Every tokio lock acquisition waits
//! under a watchdog (in every build): an ERROR (`kind = "lock_wait_wedged"`) at a third of its
//! class's bound ([`set_bound`], default [`watchdog::DEFAULT_BOUND`]), and past the bound the
//! [`set_wedge_handler`] handler, or an abort.
//!
//! # What it does not check
//!
//! * Orders no run takes — the watchdog is the answer to those in production.
//! * Two instances of one class held together (two stores' locks): no order between instances is
//!   checked. The same instance waited on again while held *is* reported.
//! * `try_lock` and its kin never wait, so they record no edge; a guard they return is held like
//!   any other.
//!
//! # Cost
//!
//! The order check is debug-only: in a release build every hook compiles away and the wrappers
//! are the inner locks plus a class name. The watchdog costs a timer per contended async wait.

#[cfg(debug_assertions)]
mod checker;
pub mod sync;
pub mod tokio_sync;
pub mod watchdog;

pub use watchdog::{set_bound, set_wedge_handler, Wedge};

use std::future::Future;
use std::panic::Location;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

/// The checker's hooks; no-ops in a release build.
pub(crate) mod hooks {
    use std::panic::Location;

    #[cfg(debug_assertions)]
    pub use crate::checker::Token;

    #[cfg(not(debug_assertions))]
    #[derive(Debug)]
    pub struct Token;

    #[inline]
    #[allow(unused_variables)]
    pub fn attempt(class: &'static str, instance: usize, site: &'static Location<'static>) {
        #[cfg(debug_assertions)]
        crate::checker::attempt(class, instance, site);
    }

    #[inline]
    #[allow(unused_variables)]
    pub fn acquired(class: &'static str, instance: usize, site: &'static Location<'static>) -> Token {
        #[cfg(debug_assertions)]
        return crate::checker::acquired(class, instance, site);
        #[cfg(not(debug_assertions))]
        Token
    }

    #[inline]
    #[allow(unused_variables)]
    pub fn released(t: &Token) {
        #[cfg(debug_assertions)]
        crate::checker::released(t);
    }

    #[inline]
    #[allow(unused_variables)]
    pub fn adopt(t: &mut Token) {
        #[cfg(debug_assertions)]
        crate::checker::adopt(t);
    }
}

/// Releases a held entry when dropped.
pub(crate) struct Held(pub(crate) hooks::Token);

impl Drop for Held {
    fn drop(&mut self) {
        hooks::released(&self.0);
    }
}

/// Every cycle reported so far — for a test harness to assert there were none.
pub fn cycles() -> Vec<String> {
    #[cfg(debug_assertions)]
    return checker::cycles();
    #[cfg(not(debug_assertions))]
    Vec::new()
}

/// Every cycle reported so far, cleared.
pub fn take_cycles() -> Vec<String> {
    #[cfg(debug_assertions)]
    return checker::take_cycles();
    #[cfg(not(debug_assertions))]
    Vec::new()
}

/// `fut`, polled as a context of its own: for a future polled concurrently with others in one
/// task (`join!`, `join_all`, `select!`), so a guard one branch holds is not read as held by the
/// branch polled after it.
pub fn branch<F: Future>(fut: F) -> Branch<F> {
    #[cfg(debug_assertions)]
    let id = checker::next_id();
    #[cfg(not(debug_assertions))]
    let id = 0;
    Branch { id, fut: Box::pin(fut) }
}

pub struct Branch<F> {
    #[cfg_attr(not(debug_assertions), allow(dead_code))]
    id: u64,
    fut: Pin<Box<F>>,
}

impl<F: Future> Future for Branch<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<F::Output> {
        #[cfg(debug_assertions)]
        {
            let id = self.id;
            checker::in_branch(id, || self.fut.as_mut().poll(cx))
        }
        #[cfg(not(debug_assertions))]
        self.fut.as_mut().poll(cx)
    }
}

/// The address a class's pseudo-lock is identified by: one instance per class.
fn pseudo(class: &'static str) -> usize {
    class.as_ptr() as usize
}

/// `fut`, run as the consumer `class`: while it runs, the consumer is held, so the locks it takes
/// order after it, and a wait on it from a holder of those locks is a cycle. For a task others
/// wait on, around each message it handles.
#[track_caller]
pub fn holding<F: Future>(class: &'static str, fut: F) -> impl Future<Output = F::Output> {
    let site = Location::caller();
    async move {
        let _held = Held(hooks::acquired(class, pseudo(class), site));
        fut.await
    }
}

/// About to wait on the consumer `class` (send it a message and await the reply, join it): its
/// order with the locks this context holds is checked, as for an acquisition. Nothing is held.
#[track_caller]
pub fn waits_on(class: &'static str) {
    hooks::attempt(class, pseudo(class), Location::caller());
}

/// `f`, run while holding a pseudo-lock of `class`: for a blocking primitive the wrappers do not
/// cover (a database's write transaction, a `OnceLock` initializer), so its order with the locks
/// around it is checked too.
#[track_caller]
pub fn scope<R>(class: &'static str, f: impl FnOnce() -> R) -> R {
    let site = Location::caller();
    hooks::attempt(class, pseudo(class), site);
    let _held = Held(hooks::acquired(class, pseudo(class), site));
    f()
}

#[cfg(all(test, debug_assertions))]
mod tests;

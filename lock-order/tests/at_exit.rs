//! A debug process that leaves a cycle on record exits non-zero (`src/at_exit.rs`): the verdict a
//! test run needs, since a cycle is never panicked on where it is taken.
//!
//! The process under test is this test binary itself, re-run with one test selected and
//! `CHILD` set — never a separately built binary, which a narrower `cargo test` would leave stale.
#![cfg(all(unix, debug_assertions))]

use std::process::{Command, Output};

const CHILD: &str = "LOCK_ORDER_AT_EXIT_CHILD";

/// The child's work: two locks taken in both orders on one thread — a cycle, though nothing
/// deadlocks. With `clear`, the cycle is taken off the record, as a harness that provokes one on
/// purpose does. Without `CHILD` set it does nothing.
#[test]
fn child_takes_a_cycle() {
    let Ok(mode) = std::env::var(CHILD) else { return };
    let a = lock_order::sync::Mutex::new("exit-a", ());
    let b = lock_order::sync::Mutex::new("exit-b", ());
    {
        let _a = a.lock().unwrap();
        let _b = b.lock().unwrap();
    }
    {
        let _b = b.lock().unwrap();
        let _a = a.lock().unwrap();
    }
    if mode == "clear" {
        lock_order::take_cycles();
    }
}

fn run_child(mode: &str) -> Output {
    let me = std::env::current_exe().expect("this test's path");
    Command::new(me)
        .args(["--exact", "child_takes_a_cycle", "--test-threads=1"])
        .env(CHILD, mode)
        .output()
        .expect("re-run this test binary")
}

#[test]
fn a_run_that_leaves_a_cycle_on_record_fails() {
    let out = run_child("keep");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(lock_order::at_exit::EXIT_STATUS), "{stderr}");
    assert!(stderr.contains("`exit-a`") && stderr.contains("`exit-b`"), "the cycle is named: {stderr}");
}

#[test]
fn a_run_that_takes_its_cycles_off_the_record_exits_clean() {
    let out = run_child("clear");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

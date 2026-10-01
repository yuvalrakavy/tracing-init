//! A debug process that leaves a cycle on record exits non-zero (`src/at_exit.rs`): the verdict a
//! test run needs, since a cycle is never panicked on where it is taken.
//!
//! The process under test is this test binary itself, re-run with one test selected and
//! `CHILD` set — never a separately built binary, which a narrower `cargo test` would leave stale.
#![cfg(all(unix, debug_assertions))]

use std::io::Write;
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const CHILD: &str = "LOCK_ORDER_AT_EXIT_CHILD";

/// SIGALRM's number on Linux and macOS alike.
const SIGALRM: i32 = 14;

/// The child's work: two locks taken in both orders on one thread — a cycle, though nothing
/// deadlocks. With `clear`, the cycle is taken off the record, as a harness that provokes one on
/// purpose does. With `fill`, stderr is first filled by a thread that keeps writing to it, so the
/// exit handler's report has nowhere to go. Without `CHILD` set it does nothing.
#[test]
fn child_takes_a_cycle() {
    let Ok(mode) = std::env::var(CHILD) else { return };
    if mode == "fill" {
        std::thread::spawn(|| {
            let chunk = [b'x'; 4096];
            let mut err = std::io::stderr();
            loop {
                // Blocks for good once the undrained pipe is full.
                let _ = err.write_all(&chunk);
            }
        });
        std::thread::sleep(Duration::from_millis(300));
    }
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

fn child(mode: &str) -> Command {
    let me = std::env::current_exe().expect("this test's path");
    let mut c = Command::new(me);
    c.args(["--exact", "child_takes_a_cycle", "--test-threads=1"]).env(CHILD, mode);
    c
}

fn run_child(mode: &str) -> Output {
    child(mode).output().expect("re-run this test binary")
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

/// Review finding (Codex, 2026-10-02): the exit handler wrote its report to fd 2 directly, but a
/// full pipe nobody drains blocks that write forever, and the process never reached `_exit` — a
/// debug server that recorded a cycle hung on its way out. The report is best-effort now: the
/// handler is bounded, and a report that cannot be written ends the process on SIGALRM, still a
/// failing exit.
#[test]
fn a_run_whose_stderr_is_full_still_exits() {
    let mut proc = child("fill")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped()) // never read: it fills
        .spawn()
        .expect("spawn this test binary");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = proc.try_wait().expect("poll the child") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = proc.kill();
            let _ = proc.wait();
            panic!("the exit handler hung on a full stderr pipe");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(!status.success(), "a run with a cycle on record must fail: {status:?}");
    assert_eq!(status.signal(), Some(SIGALRM), "the bound, not a clean exit, ended it: {status:?}");
}

//! A stdout or stderr that stops taking writes must not stall the process (Store no-hang 3b).
//!
//! A daemon's stdout and stderr are often a pipe or a socket — to a supervisor, to journald,
//! through `tee` — and when its reader stalls, every write to it blocks once the buffer is full.
//! Each test runs twice: as the parent, which starts this test binary again as a child whose
//! stream is a pipe nobody reads, and as that child (`CHILD` names the test it runs). The child
//! reports over GELF, since its stdio is the thing that is stuck.

#![cfg(all(unix, feature = "file", feature = "gelf"))]

mod common;

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{field, is, number, Gelf};
use tracing::Level;
use tracing_init::types::OnDestinationError;
use tracing_init::TracingInit;

const CHILD: &str = "TRACING_INIT_STALLED_STDIO_CHILD";
const CHILD_GELF: &str = "TRACING_INIT_STALLED_STDIO_GELF";

const STDOUT_TEST: &str = "a_stalled_stdout_drops_lines_instead_of_blocking_the_threads_that_log";
const STDERR_TEST: &str = "a_stalled_stderr_does_not_block_init";

/// Start this binary again, running `test` as the child.
fn spawn_child(test: &str, gelf: &str, stdout: Stdio, stderr: Stdio) -> Child {
    Command::new(std::env::current_exe().expect("this test binary"))
        .args([test, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env(CHILD_GELF, gelf)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .expect("start the child")
}

fn is_child(test: &str) -> bool {
    std::env::var(CHILD).as_deref() == Ok(test)
}

fn child_gelf() -> String {
    std::env::var(CHILD_GELF).expect("the parent's GELF address")
}

/// A probe the child sends so the parent can see how far it got.
fn is_probe(record: &serde_json::Value, probe: &str) -> bool {
    field(record, "probe") == Some(probe)
}

/// Wait for the child to exit, killing it past `within`.
fn reap(child: &mut Child, within: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

const THREADS: usize = 4;
const LINES_PER_THREAD: usize = 50_000;

fn stdout_child() -> ! {
    let guard = TracingInit::builder("app")
        .destination("cg")
        .gelf_address(&child_gelf())
        .level("console", Level::INFO)
        .level("gelf", Level::WARN)
        .ansi("console", false)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .expect("init");
    let (done_tx, done_rx) = mpsc::channel();
    for t in 0..THREADS {
        let done_tx = done_tx.clone();
        thread::spawn(move || {
            for i in 0..LINES_PER_THREAD {
                tracing::info!(
                    thread = t,
                    line = i,
                    "a line for a stdout nobody reads, padded out to a line's usual length"
                );
            }
            let _ = done_tx.send(t);
        });
    }
    for _ in 0..THREADS {
        let _ = done_rx.recv();
    }
    tracing::warn!(
        probe = "logging_returned",
        "the child's threads finished logging"
    );
    // Long enough for the loss to be seen and reported before the guard reports the total.
    thread::sleep(Duration::from_millis(2_500));
    drop(guard);
    tracing::warn!(probe = "guard_dropped", "the child dropped its guard");
    // Not a return: the test harness would print its verdict to the stuck stdout.
    std::process::exit(0)
}

#[test]
fn a_stalled_stdout_drops_lines_instead_of_blocking_the_threads_that_log() {
    if is_child(STDOUT_TEST) {
        stdout_child();
    }
    let mut gelf = Gelf::bind();
    let mut child = spawn_child(STDOUT_TEST, &gelf.address(), Stdio::piped(), Stdio::null());
    // Held, never read: closing it would turn the stall into a broken pipe.
    let _stdout = child.stdout.take();

    let returned = gelf.wait_for(Duration::from_secs(30), |r| is_probe(r, "logging_returned"));
    if returned.is_none() {
        let _ = reap(&mut child, Duration::ZERO);
        panic!(
            "the child's threads blocked on its stalled stdout: they did not finish their {} lines \
             within 30 s; GELF saw:\n{}",
            THREADS * LINES_PER_THREAD,
            gelf.seen()
        );
    }
    let warned = gelf.wait_for(Duration::from_secs(10), |r| {
        is(r, "log_lines_dropped", "console") && number(r, "dropped").is_some_and(|n| n > 0)
    });
    assert!(
        warned.is_some(),
        "no WARN kind=log_lines_dropped destination=console with a count reached GELF; GELF saw:\n{}",
        gelf.seen()
    );
    let dropped = gelf.wait_for(Duration::from_secs(10), |r| is_probe(r, "guard_dropped"));
    assert!(
        dropped.is_some(),
        "the child's guard did not drop within 10 s while its stdout was stalled; GELF saw:\n{}",
        gelf.seen()
    );
    let status = reap(&mut child, Duration::from_secs(10));
    assert!(
        status.is_some_and(|s| s.success()),
        "the child did not exit cleanly within 10 s of dropping its guard: {status:?}"
    );
}

fn stderr_child() -> ! {
    // Fill stderr: this thread writes until the pipe is full, then blocks holding stderr's lock.
    thread::spawn(|| {
        let line = [b'x'; 1024];
        loop {
            let _ = std::io::stderr().write_all(&line);
        }
    });
    thread::sleep(Duration::from_millis(500));
    // A file destination that cannot start, under skip: init notes the failure on stderr.
    let guard = TracingInit::builder("app")
        .destination("fg")
        .file_path("/dev/null/cannot-be-a-directory")
        .gelf_address(&child_gelf())
        .on_destination_error(OnDestinationError::Skip)
        .level("*", Level::INFO)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .expect("init under skip");
    tracing::warn!(probe = "init_returned", "the child's init returned");
    drop(guard);
    tracing::warn!(probe = "guard_dropped", "the child dropped its guard");
    std::process::exit(0)
}

#[test]
fn a_stalled_stderr_does_not_block_init() {
    if is_child(STDERR_TEST) {
        stderr_child();
    }
    let mut gelf = Gelf::bind();
    let mut child = spawn_child(STDERR_TEST, &gelf.address(), Stdio::null(), Stdio::piped());
    let _stderr = child.stderr.take();

    let returned = gelf.wait_for(Duration::from_secs(20), |r| is_probe(r, "init_returned"));
    if returned.is_none() {
        let _ = reap(&mut child, Duration::ZERO);
        panic!(
            "the child's init blocked on its stalled stderr: it did not return within 20 s; GELF saw:\n{}",
            gelf.seen()
        );
    }
    let warned = gelf.wait_for(Duration::from_secs(5), |r| {
        is(r, "log_destination_skipped", "file")
    });
    assert!(
        warned.is_some(),
        "no WARN kind=log_destination_skipped destination=file reached GELF; GELF saw:\n{}",
        gelf.seen()
    );
    let dropped = gelf.wait_for(Duration::from_secs(10), |r| is_probe(r, "guard_dropped"));
    assert!(
        dropped.is_some(),
        "the child's guard did not drop within 10 s while its stderr was stalled; GELF saw:\n{}",
        gelf.seen()
    );
    let status = reap(&mut child, Duration::from_secs(10));
    assert!(
        status.is_some_and(|s| s.success()),
        "the child did not exit cleanly within 10 s of dropping its guard: {status:?}"
    );
}

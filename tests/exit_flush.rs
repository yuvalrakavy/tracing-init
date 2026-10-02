//! `process::exit` must not lose the lines still buffered for the console and the log file
//! (Store no-hang 3b, T6).
//!
//! The console and the file are written by worker threads, and `process::exit` runs no
//! destructors, so the guard's drop never flushes them. Store's servers log an `error!` and then
//! call `process::exit` on their refusal and watchdog paths, and that line is the one that
//! matters. The test runs this binary again as a child that logs a burst, then the last line,
//! then exits at once; its stdout is a file. The last line must be in both files.

#![cfg(all(unix, feature = "file"))]

use std::fs::File;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use tracing::Level;
use tracing_init::TracingInit;

const CHILD: &str = "TRACING_INIT_EXIT_FLUSH_CHILD";
const CHILD_LOGS: &str = "TRACING_INIT_EXIT_FLUSH_LOGS";
const TEST: &str = "a_line_logged_just_before_process_exit_reaches_stdout_and_the_log_file";
/// Enough that the workers are still behind when the process exits.
const BURST: usize = 20_000;
const LAST_LINE: &str = "the last line before process exit";

fn exit_child(logs: &Path) -> ! {
    let _guard = TracingInit::builder("app")
        .destination("cf")
        .file_path(logs.to_str().expect("a UTF-8 scratch path"))
        .file_prefix("app")
        .file_rotation("n")
        .level("*", Level::INFO)
        .ansi("console", false)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .expect("init");
    for i in 0..BURST {
        tracing::info!(line = i, "a line before the last one");
    }
    tracing::error!("{LAST_LINE}");
    std::process::exit(1)
}

#[test]
fn a_line_logged_just_before_process_exit_reaches_stdout_and_the_log_file() {
    if let Some(logs) = std::env::var_os(CHILD_LOGS).filter(|_| std::env::var_os(CHILD).is_some()) {
        exit_child(Path::new(&logs));
    }
    let dir = tempfile::tempdir().expect("a scratch directory");
    let logs = dir.path().join("logs");
    let stdout_path = dir.path().join("stdout.txt");
    let stdout = File::create(&stdout_path).expect("the child's stdout file");
    let mut child = Command::new(std::env::current_exe().expect("this test binary"))
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, TEST)
        .env(CHILD_LOGS, &logs)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::null())
        .spawn()
        .expect("start the child");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Ok(Some(status)) = child.try_wait() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(
        status.and_then(|s| s.code()),
        Some(1),
        "the child did not exit with process::exit(1) within 30 s: {status:?}"
    );

    let console = std::fs::read_to_string(&stdout_path).unwrap_or_default();
    let file = std::fs::read_to_string(logs.join("app.log")).unwrap_or_default();
    let missing: Vec<&str> = [("stdout", &console), ("the log file", &file)]
        .into_iter()
        .filter(|(_, text)| !text.contains(LAST_LINE))
        .map(|(name, _)| name)
        .collect();
    assert!(
        missing.is_empty(),
        "process::exit lost the line logged just before it, from {missing:?} (stdout has {} lines, \
         the log file {})",
        console.lines().count(),
        file.lines().count()
    );
}

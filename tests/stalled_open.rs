//! A log file that cannot be opened must not stall `init` (Store no-hang 3b).
//!
//! A FIFO nobody reads at the log path makes the file's open block until a reader appears —
//! the shape of a stalled file system. Under `on_destination_error = "fail"`, `init` must return
//! the error within its bound; under `"skip"`, it must go on without the file and say so on the
//! other destinations. One test per file: `init` installs the process's global subscriber.

#![cfg(all(unix, feature = "file", feature = "gelf"))]

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use common::{field, is, mkfifo, Gelf};
use tracing_init::types::OnDestinationError;
use tracing_init::{TracingGuard, TracingInit};

fn init_in_the_background(
    dir: &std::path::Path,
    gelf: &str,
    on_error: OnDestinationError,
) -> mpsc::Receiver<Result<TracingGuard, String>> {
    let dir = dir.to_str().expect("a UTF-8 scratch path").to_string();
    let gelf = gelf.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = TracingInit::builder("app")
            .destination("fg")
            .file_path(&dir)
            .file_prefix("app")
            .file_rotation("n")
            .gelf_address(&gelf)
            .on_destination_error(on_error)
            .level("*", tracing::Level::INFO)
            .no_auto_config_file()
            .ignore_environment_variables()
            .init()
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
    });
    rx
}

#[test]
fn a_log_file_that_cannot_be_opened_does_not_stall_init() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    mkfifo(&dir.path().join("app.log"));
    let mut gelf = Gelf::bind();

    let failed = init_in_the_background(dir.path(), &gelf.address(), OnDestinationError::Fail)
        .recv_timeout(Duration::from_secs(20));
    let failed = failed.unwrap_or_else(|_| {
        panic!("init did not return within 20 s with a FIFO nobody reads at the log path (fail)")
    });
    let error = failed.expect_err("init must fail: the log file cannot be opened");
    assert!(
        error.contains("log file") && error.contains("within"),
        "the error does not say the log file's open did not finish in time: {error}"
    );

    let skipped = init_in_the_background(dir.path(), &gelf.address(), OnDestinationError::Skip)
        .recv_timeout(Duration::from_secs(20));
    let skipped = skipped.unwrap_or_else(|_| {
        panic!("init did not return within 20 s with a FIFO nobody reads at the log path (skip)")
    });
    let guard = skipped.expect("init must go on without the file under skip");
    assert!(
        guard.summary().contains("file(SKIPPED:"),
        "the summary does not record the skipped file: {}",
        guard.summary()
    );

    let warned = gelf.wait_for(Duration::from_secs(5), |r| {
        is(r, "log_destination_skipped", "file")
    });
    let warned = warned.unwrap_or_else(|| {
        panic!(
            "no WARN kind=log_destination_skipped destination=file reached GELF; GELF saw:\n{}",
            gelf.seen()
        )
    });
    assert_eq!(warned["level"], 4, "the skip is reported at WARN: {warned}");
    assert!(
        field(&warned, "error").is_some_and(|e| e.contains("within")),
        "the WARN does not carry why the file was skipped: {warned}"
    );
}

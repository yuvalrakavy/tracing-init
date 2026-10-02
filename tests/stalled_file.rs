//! A log file that stops taking writes must not stall the threads that log (Store no-hang 3b).
//!
//! The log file is a FIFO whose reader opens it and never reads, so once the pipe's buffer is
//! full every write to it blocks — the shape of a stalled file system. Logging from several
//! threads must keep returning, the lines the file cannot take must be counted, the loss must be
//! reported on another destination (GELF here), and dropping the guard must return within its
//! bound. One test per file: `init` installs the process's global subscriber.

#![cfg(all(unix, feature = "file", feature = "gelf"))]

mod common;

use std::fs::File;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{is, mkfifo, number, Gelf};
use tracing::Level;

const THREADS: usize = 4;
const LINES_PER_THREAD: usize = 50_000;
const LINES: u64 = (THREADS * LINES_PER_THREAD) as u64;
/// What the FIFO can hold before its writer blocks: 64 KiB of ~150-byte lines is ~450.
/// Generous, since it only widens the lower bound on what must have been dropped.
const PIPE_LINES: u64 = 2_000;

#[test]
fn a_stalled_log_file_drops_lines_instead_of_blocking_the_threads_that_log() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    // Rotation "n" (never) names the file `<prefix>.log`.
    let fifo = dir.path().join("app.log");
    mkfifo(&fifo);

    // The reader's open is what lets the writer's open return; it then never reads.
    let (reader_tx, reader_rx) = mpsc::channel();
    let reader_path = fifo.clone();
    thread::spawn(move || {
        let reader = File::open(&reader_path).expect("open the FIFO for reading");
        let _ = reader_tx.send(reader);
    });

    let mut gelf = Gelf::bind();
    let guard = tracing_init::TracingInit::builder("app")
        .destination("fg")
        .file_path(dir.path().to_str().expect("a UTF-8 scratch path"))
        .file_prefix("app")
        .file_rotation("n")
        .gelf_address(&gelf.address())
        .level("file", Level::INFO)
        .level("gelf", Level::WARN)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .expect("init");
    let _reader = reader_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("the FIFO's reader opened it");

    let (done_tx, done_rx) = mpsc::channel();
    for t in 0..THREADS {
        let done_tx = done_tx.clone();
        thread::spawn(move || {
            for i in 0..LINES_PER_THREAD {
                tracing::info!(
                    thread = t,
                    line = i,
                    "a line for a log file that has stopped taking writes, padded out to a line's usual length"
                );
            }
            let _ = done_tx.send(t);
        });
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut finished = 0;
    while finished < THREADS {
        match done_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(_) => finished += 1,
            Err(_) => break,
        }
    }
    assert_eq!(
        finished, THREADS,
        "the threads that log blocked on the stalled log file: {finished} of {THREADS} finished their \
         {LINES_PER_THREAD} lines within 30 s"
    );

    let warned = gelf.wait_for(Duration::from_secs(10), |r| {
        is(r, "log_lines_dropped", "file") && number(r, "dropped").is_some()
    });
    let warned = warned.unwrap_or_else(|| {
        panic!(
            "no WARN kind=log_lines_dropped destination=file reached GELF within 10 s; GELF saw:\n{}",
            gelf.seen()
        )
    });
    assert_eq!(warned["level"], 4, "the loss is reported at WARN: {warned}");
    let dropped = number(&warned, "dropped").unwrap_or(0);
    assert!(dropped > 0, "the WARN counts no dropped line: {warned}");

    let (dropped_tx, dropped_rx) = mpsc::channel();
    let started = Instant::now();
    thread::spawn(move || {
        drop(guard);
        let _ = dropped_tx.send(());
    });
    let returned = dropped_rx.recv_timeout(Duration::from_secs(15));
    let took = started.elapsed();
    assert!(
        returned.is_ok() && took < Duration::from_secs(6),
        "dropping the guard did not return within its bound while the log file was stalled: {:?} after {took:?}",
        returned
    );

    let report = gelf.wait_for(Duration::from_secs(5), |r| {
        is(r, "log_lines_dropped", "file") && number(r, "dropped_total").is_some()
    });
    let report = report.unwrap_or_else(|| {
        panic!(
            "dropping the guard reported no total for the file; GELF saw:\n{}",
            gelf.seen()
        )
    });
    let total = number(&report, "dropped_total").unwrap_or(0);
    // Every line logged is written, buffered or dropped (the reports themselves add a line or two).
    let buffered = tracing_appender::non_blocking::DEFAULT_BUFFERED_LINES_LIMIT as u64;
    assert!(
        total >= dropped && total >= LINES - buffered - PIPE_LINES && total <= LINES + 3,
        "the total dropped ({total}) is not what the stalled file could not take: {LINES} lines logged, \
         {buffered} buffered, at most {PIPE_LINES} written; the WARN counted {dropped}"
    );
    println!("the guard's drop took {took:?}\nthe WARN: {warned}\nthe total: {report}");
}

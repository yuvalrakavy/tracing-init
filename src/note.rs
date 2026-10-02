//! tracing-init's own notes on stderr — a destination that could not start, the OTel collector
//! going offline — written without waiting on stderr.
//!
//! stderr is often a pipe (a supervisor, journald, `tee`), and a write to a pipe whose reader
//! has stalled blocks once the pipe's buffer is full. So the notes are written by a thread of
//! their own through a small lossy buffer: a note that does not fit is dropped (and counted by
//! the buffer), never waited for. The buffer is made with the first note, and flushed, within
//! its bound, when the [`TracingGuard`](crate::TracingGuard) is dropped.

use std::io::Write;
use std::sync::{Mutex, OnceLock};

use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};

/// A process writes a handful of notes; the buffer only has to ride out a stall.
const NOTES_BUFFERED: usize = 256;

struct Notes {
    writer: NonBlocking,
    worker: Mutex<Option<WorkerGuard>>,
}

static NOTES: OnceLock<Notes> = OnceLock::new();

fn notes() -> &'static Notes {
    NOTES.get_or_init(|| {
        let (writer, worker) = NonBlockingBuilder::default()
            .lossy(true)
            .buffered_lines_limit(NOTES_BUFFERED)
            .thread_name("tracing-init-stderr")
            .finish(std::io::stderr());
        Notes {
            writer,
            worker: Mutex::new(Some(worker)),
        }
    })
}

/// Write one line to stderr without waiting on stderr.
pub(crate) fn note(line: std::fmt::Arguments<'_>) {
    let mut text = line.to_string();
    text.push('\n');
    let mut writer = notes().writer.clone();
    // A lossy writer: a full buffer drops the note and counts it, and never returns an error.
    let _ = writer.write_all(text.as_bytes());
}

/// The notes' worker, for the guard to flush; `None` when no note was ever written.
pub(crate) fn take_worker() -> Option<WorkerGuard> {
    NOTES
        .get()?
        .worker
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

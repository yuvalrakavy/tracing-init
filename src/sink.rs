//! A destination that stops taking writes must not stall the threads that log.
//!
//! The console and the log file are written by a worker thread of their own, through
//! tracing-appender's lossy non-blocking writer: a thread that logs hands its line to a bounded
//! buffer and returns, and a line that does not fit is dropped and counted, never waited for. A
//! file on a stalled file system, a FIFO nobody reads, a stdout pipe whose reader stopped —
//! each costs lines, never a thread. GELF sends on a non-blocking socket for the same reason
//! (`gelf.rs`).
//!
//! Loss is never silent. Every destination counts the lines it delivered and the lines it lost
//! (no room in its buffer, or a failed write), and a monitor thread turns the counts into
//! records, one of each per episode:
//!
//! - WARN `kind = "log_lines_dropped"`, "log destination is dropping lines" (`destination`,
//!   `dropped`), when a destination starts losing lines;
//! - INFO, the same kind, "log destination stopped dropping lines" (`destination`, `dropped` in
//!   the episode, `lasted_ms` from the WARN), once it has delivered lines again and lost none
//!   for [`QUIET`]. A destination that delivers nothing — a stuck file — stays in its episode;
//! - when the guard is dropped, for each destination that lost any line, "log destination
//!   dropped lines during this run" (`destination`, `dropped_total`, `still_dropping`): WARN
//!   while it is still losing lines, INFO otherwise.
//!
//! The records go through the subscriber like any other, so the destinations that still work
//! carry them; the stuck one drops them as well, each one more lost line. The monitor cannot
//! be stalled by a stuck destination: every destination it writes to only hands lines over.

use std::io::{self, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tracing_appender::non_blocking::{
    ErrorCounter, NonBlocking, NonBlockingBuilder, WorkerGuard, DEFAULT_BUFFERED_LINES_LIMIT,
};

/// Lines a stream destination holds while its stream is slow or stuck; past it, lines are
/// dropped. tracing-appender's default: seconds of a busy process, about 30 MB of 250-byte
/// lines.
pub(crate) const BUFFERED_LINES: usize = DEFAULT_BUFFERED_LINES_LIMIT;

/// How often the monitor reads the counts.
const TICK: Duration = Duration::from_secs(1);

/// How long a destination in an episode must go without losing a line, having delivered
/// one, before the episode is over. Long enough that a burst-by-burst overload is one episode.
pub(crate) const QUIET: Duration = Duration::from_secs(10);

/// How long the guard waits for the monitor to stop: a tick's work, which never waits.
const STOP_BOUND: Duration = Duration::from_millis(500);

/// How long the guard waits for the stream destinations to flush. tracing-appender's own
/// `WorkerGuard::drop` takes at most 1.1 s (see [`flush_within`]).
pub(crate) const FLUSH_BOUND: Duration = Duration::from_millis(1_500);

/// What a destination did with the lines given to it.
#[derive(Debug, Default)]
pub(crate) struct LineCounts {
    delivered: AtomicU64,
    lost: AtomicU64,
}

impl LineCounts {
    pub(crate) fn note_delivered(&self) {
        self.delivered.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn note_lost(&self) {
        self.lost.fetch_add(1, Ordering::Relaxed);
    }
}

/// One destination's counts, as the monitor reads them.
#[derive(Debug, Clone)]
pub(crate) struct Watch {
    destination: &'static str,
    counts: Arc<LineCounts>,
    /// Lines the writer's buffer had no room for (the stream destinations).
    full: Option<ErrorCounter>,
}

impl Watch {
    pub(crate) fn new(
        destination: &'static str,
        counts: Arc<LineCounts>,
        full: Option<ErrorCounter>,
    ) -> Self {
        Watch {
            destination,
            counts,
            full,
        }
    }

    pub(crate) fn lost(&self) -> u64 {
        let full = self.full.as_ref().map_or(0, |c| c.dropped_lines() as u64);
        self.counts.lost.load(Ordering::Relaxed) + full
    }

    pub(crate) fn delivered(&self) -> u64 {
        self.counts.delivered.load(Ordering::Relaxed)
    }
}

/// A destination's writing side: what watches it for loss, and the worker that writes it.
pub(crate) struct Sink {
    pub(crate) watch: Watch,
    pub(crate) worker: Option<WorkerGuard>,
}

/// A lossy, bounded writer for `stream`, written by a thread of its own.
pub(crate) fn lossy<W: Write + Send + 'static>(
    destination: &'static str,
    stream: W,
) -> (NonBlocking, Sink) {
    let counts = Arc::new(LineCounts::default());
    let (writer, worker) = NonBlockingBuilder::default()
        .lossy(true)
        .buffered_lines_limit(BUFFERED_LINES)
        .thread_name(&format!("tracing-init-{destination}"))
        .finish(CountingWriter {
            inner: stream,
            counts: counts.clone(),
        });
    let watch = Watch::new(destination, counts, Some(writer.error_counter()));
    let sink = Sink {
        watch,
        worker: Some(worker),
    };
    (writer, sink)
}

/// Counts, on the worker thread, each line its stream took and each it refused.
///
/// tracing-appender's worker writes each line with one `write_all` and ignores a failure, so a
/// full disk or a closed pipe would otherwise lose lines without a count.
struct CountingWriter<W> {
    inner: W,
    counts: Arc<LineCounts>,
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn write_all(&mut self, line: &[u8]) -> io::Result<()> {
        let written = self.inner.write_all(line);
        match written {
            Ok(()) => self.counts.note_delivered(),
            Err(_) => self.counts.note_lost(),
        }
        written
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// A loss episode: from the first lost line the monitor saw to the quiet that ends it.
#[derive(Debug)]
struct Episode {
    started: Instant,
    lost_before: u64,
    last_loss: Instant,
    delivered_at_last_loss: u64,
}

/// The monitor's view of one destination.
#[derive(Debug)]
pub(crate) struct Watched {
    watch: Watch,
    lost_seen: u64,
    episode: Option<Episode>,
}

impl Watched {
    pub(crate) fn new(watch: Watch) -> Self {
        Watched {
            watch,
            lost_seen: 0,
            episode: None,
        }
    }

    /// Read the counts; open or close an episode, and say so.
    pub(crate) fn tick(&mut self, now: Instant) {
        let lost = self.watch.lost();
        let delivered = self.watch.delivered();
        let destination = self.watch.destination;
        match &mut self.episode {
            None if lost > self.lost_seen => {
                self.episode = Some(Episode {
                    started: now,
                    lost_before: self.lost_seen,
                    last_loss: now,
                    delivered_at_last_loss: delivered,
                });
                tracing::warn!(
                    kind = "log_lines_dropped",
                    destination,
                    dropped = lost - self.lost_seen,
                    "log destination is dropping lines"
                );
            }
            None => {}
            Some(episode) if lost > self.lost_seen => {
                episode.last_loss = now;
                episode.delivered_at_last_loss = delivered;
            }
            Some(episode)
                if delivered > episode.delivered_at_last_loss
                    && now.saturating_duration_since(episode.last_loss) >= QUIET =>
            {
                let dropped = lost - episode.lost_before;
                let lasted_ms = now.saturating_duration_since(episode.started).as_millis() as u64;
                self.episode = None;
                tracing::info!(
                    kind = "log_lines_dropped",
                    destination,
                    dropped,
                    lasted_ms,
                    "log destination stopped dropping lines"
                );
            }
            Some(_) => {}
        }
        // Read again, after this tick's own record: a stuck destination drops that too, and
        // the drop is this record's, not a new loss.
        self.lost_seen = self.watch.lost();
    }

    /// Report, once more, a destination that lost any line during the run.
    pub(crate) fn report_total(&self) {
        let dropped_total = self.watch.lost();
        if dropped_total == 0 {
            return;
        }
        let destination = self.watch.destination;
        let still_dropping = self.episode.is_some() || dropped_total > self.lost_seen;
        if still_dropping {
            tracing::warn!(
                kind = "log_lines_dropped",
                destination,
                dropped_total,
                still_dropping,
                "log destination dropped lines during this run"
            );
        } else {
            tracing::info!(
                kind = "log_lines_dropped",
                destination,
                dropped_total,
                still_dropping,
                "log destination dropped lines during this run"
            );
        }
    }
}

/// The thread that turns the destinations' counts into records.
pub(crate) struct Monitor {
    /// Dropped to stop the thread.
    stop: mpsc::Sender<()>,
    /// The thread's view of each destination, sent back when it stops. In a mutex only so the
    /// guard that holds the monitor stays `Sync`; it is never contended.
    done: Mutex<mpsc::Receiver<Vec<Watched>>>,
    watches: Vec<Watch>,
}

impl Monitor {
    /// Start watching `watches`; `None` when there is nothing to watch.
    pub(crate) fn start(watches: Vec<Watch>) -> Option<Monitor> {
        if watches.is_empty() {
            return None;
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let (done_tx, done) = mpsc::channel();
        let mut watched: Vec<Watched> = watches.iter().cloned().map(Watched::new).collect();
        let spawned = thread::Builder::new()
            .name("tracing-init-loss".into())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(TICK) {
                    let now = Instant::now();
                    for w in &mut watched {
                        w.tick(now);
                    }
                }
                let _ = done_tx.send(watched);
            });
        if let Err(e) = spawned {
            // The counts still count; the guard's drop still reports them.
            crate::note::note(format_args!(
                "tracing-init: the log-loss monitor could not start: {e} — lost lines are reported only at shutdown"
            ));
        }
        Some(Monitor {
            stop,
            done: Mutex::new(done),
            watches,
        })
    }

    /// Stop the thread, and report the total of every destination that lost lines.
    pub(crate) fn finish(self) {
        let Monitor {
            stop,
            done,
            watches,
        } = self;
        drop(stop);
        let done = done
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let watched = match done.recv_timeout(STOP_BOUND) {
            Ok(watched) => watched,
            // No thread, or one that did not stop in time: every loss is reported as ongoing.
            Err(_) => watches.into_iter().map(Watched::new).collect(),
        };
        for w in &watched {
            w.report_total();
        }
    }
}

/// Flush the stream destinations' workers, each on a thread of its own, waiting at most
/// `bound` for all of them.
///
/// tracing-appender's `WorkerGuard::drop` (0.2.3 to 0.2.5) waits at most 100 ms to hand its
/// worker the shutdown and then at most 1 s for the worker to finish writing. But when the
/// hand-over times out, which is what a full buffer makes it do, it `println!`s to stdout, and
/// that waits without bound on a stalled stdout — or on stdout's lock, held by the console's own
/// worker while it is blocked writing. So each drop runs on a thread of its own; one still
/// running at the bound is abandoned, to end with the process.
pub(crate) fn flush_within(workers: Vec<WorkerGuard>, bound: Duration) {
    let (done_tx, done) = mpsc::channel::<()>();
    let mut flushing = 0;
    for worker in workers {
        let done_tx = done_tx.clone();
        let spawned = thread::Builder::new()
            .name("tracing-init-flush".into())
            .spawn(move || {
                drop(worker);
                let _ = done_tx.send(());
            });
        // A thread that could not start dropped its worker here, within tracing-appender's
        // own bound except for that `println!`.
        if spawned.is_ok() {
            flushing += 1;
        }
    }
    drop(done_tx);
    let deadline = Instant::now() + bound;
    for _ in 0..flushing {
        let left = deadline.saturating_duration_since(Instant::now());
        if done.recv_timeout(left).is_err() {
            break;
        }
    }
}

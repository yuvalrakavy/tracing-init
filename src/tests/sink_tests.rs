//! The loss monitor's episodes, and the counts it reads.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

use crate::sink::{self, LineCounts, Watch, Watched, QUIET};

#[derive(Debug, Clone)]
struct Record {
    level: Level,
    fields: Vec<(String, String)>,
}

impl Record {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct Fields(Vec<(String, String)>);

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0
            .push((field.name().to_string(), format!("{value:?}")));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.push((field.name().to_string(), value.to_string()));
    }
}

/// Keeps every record; with `drops_each_record`, also stands in for a stuck destination that
/// drops each record it is given.
#[derive(Clone, Default)]
struct Capture {
    records: Arc<Mutex<Vec<Record>>>,
    drops_each_record: Option<Arc<LineCounts>>,
}

impl Capture {
    fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        event.record(&mut fields);
        self.records.lock().unwrap().push(Record {
            level: *event.metadata().level(),
            fields: fields.0,
        });
        if let Some(counts) = &self.drops_each_record {
            counts.note_lost();
        }
    }
}

fn watched(counts: &Arc<LineCounts>) -> Watched {
    Watched::new(Watch::new("file", counts.clone(), None))
}

fn lose(counts: &LineCounts, n: usize) {
    for _ in 0..n {
        counts.note_lost();
    }
}

fn with_capture(capture: &Capture, f: impl FnOnce()) {
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    tracing::subscriber::with_default(subscriber, f);
}

#[test]
fn a_loss_opens_one_episode_with_one_warn() {
    let capture = Capture::default();
    let counts = Arc::new(LineCounts::default());
    let mut w = watched(&counts);
    let t0 = Instant::now();
    with_capture(&capture, || {
        w.tick(t0);
        lose(&counts, 5);
        w.tick(t0 + Duration::from_secs(1));
        lose(&counts, 3);
        w.tick(t0 + Duration::from_secs(2));
    });
    let records = capture.records();
    assert_eq!(records.len(), 1, "one record per episode: {records:?}");
    let warn = &records[0];
    assert_eq!(warn.level, Level::WARN);
    assert_eq!(warn.field("kind"), Some("log_lines_dropped"));
    assert_eq!(warn.field("destination"), Some("file"));
    assert_eq!(warn.field("dropped"), Some("5"));
    assert_eq!(
        warn.field("message"),
        Some("log destination is dropping lines")
    );
}

#[test]
fn a_destination_that_delivers_nothing_stays_in_its_episode() {
    let capture = Capture::default();
    let counts = Arc::new(LineCounts::default());
    let mut w = watched(&counts);
    let t0 = Instant::now();
    with_capture(&capture, || {
        lose(&counts, 1);
        w.tick(t0);
        // Nothing lost since, and nothing delivered: a stuck file that nobody logs to.
        w.tick(t0 + QUIET * 3);
    });
    let records = capture.records();
    assert_eq!(
        records.len(),
        1,
        "a destination that delivered nothing has not recovered: {records:?}"
    );
}

#[test]
fn an_episode_ends_once_lines_are_delivered_and_none_lost_for_the_quiet_period() {
    let capture = Capture::default();
    let counts = Arc::new(LineCounts::default());
    let mut w = watched(&counts);
    let t0 = Instant::now();
    let second = Duration::from_secs(1);
    with_capture(&capture, || {
        lose(&counts, 2);
        w.tick(t0);
        lose(&counts, 1);
        w.tick(t0 + second);
        counts.note_delivered();
        w.tick(t0 + QUIET);
        assert_eq!(
            capture.records().len(),
            1,
            "the quiet period runs from the last loss"
        );
        w.tick(t0 + second + QUIET);
        lose(&counts, 4);
        w.tick(t0 + second * 2 + QUIET);
    });
    let records = capture.records();
    assert_eq!(
        records.len(),
        3,
        "WARN, INFO, then a new episode's WARN: {records:?}"
    );
    let info = &records[1];
    assert_eq!(info.level, Level::INFO);
    assert_eq!(info.field("kind"), Some("log_lines_dropped"));
    assert_eq!(
        info.field("message"),
        Some("log destination stopped dropping lines")
    );
    assert_eq!(info.field("dropped"), Some("3"), "the episode's own count");
    let lasted = (second + QUIET).as_millis().to_string();
    assert_eq!(info.field("lasted_ms"), Some(lasted.as_str()));
    assert_eq!(records[2].level, Level::WARN);
    assert_eq!(records[2].field("dropped"), Some("4"));
}

#[test]
fn the_monitors_own_record_dropped_by_the_stuck_destination_is_not_a_new_loss() {
    let counts = Arc::new(LineCounts::default());
    let capture = Capture {
        drops_each_record: Some(counts.clone()),
        ..Capture::default()
    };
    let mut w = watched(&counts);
    let t0 = Instant::now();
    with_capture(&capture, || {
        lose(&counts, 1);
        // The WARN this tick writes is dropped by the stuck destination too.
        w.tick(t0);
        counts.note_delivered();
        w.tick(t0 + QUIET);
    });
    let records = capture.records();
    assert_eq!(
        records.len(),
        2,
        "the WARN's own drop restarted the quiet period: {records:?}"
    );
    assert_eq!(records[1].level, Level::INFO);
    assert_eq!(
        records[1].field("dropped"),
        Some("2"),
        "the episode counts its WARN's drop"
    );
}

#[test]
fn the_total_is_reported_once_more_at_the_end() {
    let capture = Capture::default();
    let t0 = Instant::now();

    let clean = Arc::new(LineCounts::default());
    let stuck = Arc::new(LineCounts::default());
    let recovered = Arc::new(LineCounts::default());
    let mut clean_w = watched(&clean);
    let mut stuck_w = watched(&stuck);
    let mut recovered_w = watched(&recovered);
    with_capture(&capture, || {
        clean.note_delivered();
        clean_w.tick(t0);
        lose(&stuck, 7);
        stuck_w.tick(t0);
        lose(&recovered, 2);
        recovered_w.tick(t0);
        recovered.note_delivered();
        recovered_w.tick(t0 + QUIET);
    });
    let before = capture.records().len();
    with_capture(&capture, || {
        clean_w.report_total();
        stuck_w.report_total();
        recovered_w.report_total();
    });
    let totals: Vec<Record> = capture.records().split_off(before);
    assert_eq!(
        totals.len(),
        2,
        "one total per destination that lost lines: {totals:?}"
    );
    let still = &totals[0];
    assert_eq!(
        still.level,
        Level::WARN,
        "a destination still dropping ends the run at WARN: {still:?}"
    );
    assert_eq!(still.field("dropped_total"), Some("7"));
    assert_eq!(still.field("still_dropping"), Some("true"));
    assert_eq!(
        still.field("message"),
        Some("log destination dropped lines during this run")
    );
    let over = &totals[1];
    assert_eq!(over.level, Level::INFO);
    assert_eq!(over.field("dropped_total"), Some("2"));
    assert_eq!(over.field("still_dropping"), Some("false"));
}

/// A stream that takes every line, or refuses every line.
struct Stream {
    taken: Arc<Mutex<Vec<u8>>>,
    refuses: bool,
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.refuses {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        self.taken.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_lines(refuses: bool) -> (Watch, Vec<u8>) {
    let taken = Arc::new(Mutex::new(Vec::new()));
    let stream = Stream {
        taken: taken.clone(),
        refuses,
    };
    let (mut writer, sink) = sink::lossy("test", stream);
    for _ in 0..3 {
        writer.write_all(b"a line\n").unwrap();
    }
    // The worker writes every queued line before it takes the shutdown.
    sink::flush_within(sink.worker.into_iter().collect(), Duration::from_secs(5));
    let taken = taken.lock().unwrap().clone();
    (sink.watch, taken)
}

#[test]
fn a_stream_that_refuses_a_line_loses_it_and_the_loss_is_counted() {
    let (watch, taken) = write_lines(true);
    assert!(taken.is_empty());
    assert_eq!(watch.lost(), 3, "every refused line is a lost line");
    assert_eq!(watch.delivered(), 0);

    let (watch, taken) = write_lines(false);
    assert_eq!(taken, b"a line\na line\na line\n".to_vec());
    assert_eq!(watch.lost(), 0);
    assert_eq!(watch.delivered(), 3);
}

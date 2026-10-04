//! The log of one bridge, whatever thread it logs from.
//!
//! A bridge logs from its MQTT tasks (the test's runtime threads) and from its device worker's
//! thread, all inside its `bridge` span, whose `instance` field names it. One global recorder —
//! installed once, so no callsite's interest is cached against another test's subscriber — routes
//! each event to the capture of the instance it was logged under.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, Once, OnceLock};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;

/// One log event: its level, its `kind` (empty when it has none), and every field as text.
#[derive(Debug, Clone)]
pub struct Record {
    pub level: Level,
    pub kind: String,
    pub fields: BTreeMap<String, String>,
}

impl Record {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields.get(name).map(String::as_str)
    }
}

type Sinks = Mutex<HashMap<String, Arc<Mutex<Vec<Record>>>>>;

fn sinks() -> &'static Sinks {
    static SINKS: OnceLock<Sinks> = OnceLock::new();
    SINKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let _ = tracing::subscriber::set_global_default(tracing_subscriber::registry().with(Recorder));
    });
    tracing_core::callsite::rebuild_interest_cache();
}

/// The log of the bridge whose instance is `instance`, from `start` until dropped.
pub struct Capture {
    instance: String,
    records: Arc<Mutex<Vec<Record>>>,
}

impl Capture {
    pub fn start(instance: &str) -> Capture {
        install();
        let records = Arc::new(Mutex::new(Vec::new()));
        sinks().lock().unwrap_or_else(|p| p.into_inner()).insert(instance.to_owned(), records.clone());
        Capture { instance: instance.to_owned(), records }
    }

    /// Every event so far, in order.
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The events so far that carry `kind`.
    pub fn of_kind(&self, kind: &str) -> Vec<Record> {
        self.records().into_iter().filter(|r| r.kind == kind).collect()
    }

    /// The events so far at `level` or more severe.
    pub fn at_least(&self, level: Level) -> Vec<Record> {
        self.records().into_iter().filter(|r| r.level <= level).collect()
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        sinks().lock().unwrap_or_else(|p| p.into_inner()).remove(&self.instance);
    }
}

/// A span's `instance`, kept in its extensions.
struct Instance(String);

struct Recorder;

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for Recorder {
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut fields = Fields::default();
        attrs.record(&mut fields);
        if let (Some(instance), Some(span)) = (fields.0.remove("instance"), ctx.span(id)) {
            span.extensions_mut().insert(Instance(instance));
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let Some(scope) = ctx.event_scope(event) else {
            return;
        };
        let Some(instance) = scope.into_iter().find_map(|span| span.extensions().get::<Instance>().map(|i| i.0.clone())) else {
            return;
        };
        let Some(sink) = sinks().lock().unwrap_or_else(|p| p.into_inner()).get(&instance).cloned() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        let kind = fields.0.get("kind").cloned().unwrap_or_default();
        sink.lock().unwrap_or_else(|p| p.into_inner()).push(Record { level: *event.metadata().level(), kind, fields: fields.0 });
    }
}

#[derive(Default)]
struct Fields(BTreeMap<String, String>);

impl Visit for Fields {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().to_owned(), value.to_owned());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().to_owned(), format!("{value:?}"));
    }
}

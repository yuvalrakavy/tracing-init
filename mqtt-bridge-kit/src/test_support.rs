//! Test doubles shared by the runtime's unit tests (lifted from mqtt_ac's `test_support.rs`).

#![allow(dead_code)]

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};

use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

/// A TCP relay between the bridge and the broker that a test can cut, as a network fault would:
/// the bridge sees its connection drop, and reconnects through the relay. It can also refuse
/// connections, as a broker that is down would, or drop each one a while after it is made, as a
/// broker that takes connections and loses them does.
///
/// It takes at most `RELAY_CAP` connections, refused or not, and then stops listening: a bridge
/// that reconnects in a loop is refused by the operating system from then on, and leaves no
/// TIME_WAIT socket behind for each attempt (an uncapped spin used up this host's ephemeral ports).
pub struct Relay {
    pub address: String,
    links: Arc<Mutex<Vec<JoinHandle<()>>>>,
    refusing: Arc<AtomicBool>,
    /// Each connection made from now on is dropped this long after it is made (ms; 0: never).
    drop_after_ms: Arc<AtomicUsize>,
    accepted: Arc<AtomicUsize>,
    task: JoinHandle<()>,
}

/// The connections a relay takes at most (see `Relay`).
pub const RELAY_CAP: usize = 100;

impl Drop for Relay {
    fn drop(&mut self) {
        self.task.abort();
        self.cut();
    }
}

impl Relay {
    pub async fn start(upstream: String) -> Relay {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the relay");
        let address = listener.local_addr().expect("the relay's address").to_string();
        let links = Arc::new(Mutex::new(Vec::new()));
        let held = links.clone();
        let refusing = Arc::new(AtomicBool::new(false));
        let refuse = refusing.clone();
        let drop_after_ms = Arc::new(AtomicUsize::new(0));
        let drop_after = drop_after_ms.clone();
        let accepted = Arc::new(AtomicUsize::new(0));
        let counted = accepted.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                if counted.fetch_add(1, Ordering::SeqCst) + 1 >= RELAY_CAP {
                    return; // the listener is dropped: refused from now on
                }
                if refuse.load(Ordering::SeqCst) {
                    drop(client);
                    continue;
                }
                let upstream = upstream.clone();
                let lifetime = match drop_after.load(Ordering::SeqCst) {
                    0 => None,
                    ms => Some(std::time::Duration::from_millis(ms as u64)),
                };
                let link = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = server.set_nodelay(true);
                        let _ = client.set_nodelay(true);
                        let relayed = tokio::io::copy_bidirectional(&mut client, &mut server);
                        match lifetime {
                            Some(lifetime) => {
                                let _ = tokio::time::timeout(lifetime, relayed).await;
                            }
                            None => {
                                let _ = relayed.await;
                            }
                        }
                    }
                });
                held.lock().unwrap().push(link);
            }
        });
        Relay { address, links, refusing, drop_after_ms, accepted, task }
    }

    /// Drop every open connection through the relay.
    pub fn cut(&self) {
        for link in self.links.lock().unwrap().drain(..) {
            link.abort();
        }
    }

    /// Refuse every new connection from now on (a broker that is down), or take them again.
    pub fn refuse(&self, refusing: bool) {
        self.refusing.store(refusing, Ordering::SeqCst);
    }

    /// Drop each connection made from now on `after` it is made (a broker that takes connections
    /// and loses them), or (`None`) keep them.
    pub fn drop_connections_after(&self, after: Option<std::time::Duration>) {
        let ms = after.map_or(0, |after| after.as_millis().max(1) as usize);
        self.drop_after_ms.store(ms, Ordering::SeqCst);
    }

    /// How many connections the relay has taken, refused or not.
    pub fn accepted(&self) -> usize {
        self.accepted.load(Ordering::SeqCst)
    }
}

/// One log event: its level, its `kind` field (empty when it has none), and every field (the
/// message as `message`), as text.
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

/// Where this thread's log events go while a test watches them.
type Sink = Rc<dyn Fn(Record)>;

thread_local! {
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// The tests' recorder is the process's global subscriber, installed once; each test watches its
/// own thread's events through it (`watch`). A subscriber per test (`set_default`) lost events:
/// tracing caches each callsite's interest when the callsite is first hit, computed from the
/// subscribers registered at that moment — while only one is, from the hitting thread's own — so
/// a callsite first hit by another test's thread, as this test's subscriber was being registered,
/// cached "never" for good, and this test never saw that event. With one global subscriber the
/// cache can be stale only for a callsite hit while it was being installed: after a pause for any
/// such registration to finish, the cache is rebuilt, as it is on every `watch`.
fn install() {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        let recorder = tracing_subscriber::registry().with(Recorder);
        let _ = tracing::subscriber::set_global_default(recorder);
        std::thread::sleep(std::time::Duration::from_millis(10));
    });
    tracing_core::callsite::rebuild_interest_cache();
}

/// Hand every log event of this thread to `on_event`, as it is emitted, until the guard is
/// dropped. A test using it runs on a current-thread runtime (the `#[tokio::test]` default), where
/// every task it spawns runs on the test's thread too.
pub fn watch(on_event: impl Fn(Record) + 'static) -> Watch {
    install();
    SINK.with(|sink| *sink.borrow_mut() = Some(Rc::new(on_event)));
    Watch(())
}

/// Stops `watch` when dropped.
pub struct Watch(());

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = SINK.try_with(|sink| sink.borrow_mut().take());
    }
}

/// The log events of this thread from `start` until it is dropped (see `watch`).
pub struct Capture {
    records: Arc<Mutex<Vec<Record>>>,
    _watch: Watch,
}

impl Capture {
    pub fn start() -> Capture {
        let records = Arc::new(Mutex::new(Vec::new()));
        let sink = records.clone();
        let _watch = watch(move |record| sink.lock().unwrap().push(record));
        Capture { records, _watch }
    }

    /// Every event so far, in order.
    pub fn records(&self) -> Vec<Record> {
        self.records.lock().unwrap().clone()
    }

    /// The events so far that carry `kind`.
    pub fn of_kind(&self, kind: &str) -> Vec<Record> {
        self.records().into_iter().filter(|r| r.kind == kind).collect()
    }

    /// The events so far at `level` or more severe (ERROR is the most severe).
    pub fn at_least(&self, level: Level) -> Vec<Record> {
        self.records().into_iter().filter(|r| r.level <= level).collect()
    }
}

struct Recorder;

impl<S: Subscriber> Layer<S> for Recorder {
    fn on_event(&self, event: &Event<'_>, _: Context<'_, S>) {
        let Some(sink) = SINK.try_with(|sink| sink.borrow().clone()).ok().flatten() else {
            return;
        };
        let mut fields = Fields::default();
        event.record(&mut fields);
        let kind = fields.0.get("kind").cloned().unwrap_or_default();
        sink(Record { level: *event.metadata().level(), kind, fields: fields.0 });
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

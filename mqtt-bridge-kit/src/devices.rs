//! The device runtimes' life (multi-device spec §4.1, §4.3, §4.4): a driver started as R1's
//! worker, its watchdog and the keeper that watches both, and a driver's construction off to the
//! side — for a fixed bridge's one device and for each device of a multi-device host.
//!
//! **The keeper.** Each device's worker has a keeper task that waits on its watchdog's verdict, the
//! worker thread's end, and a stop request. A verdict, or an end nobody asked for (a panic), is a
//! death: sent to the process's death channel, which [`Running::ended`](crate::Running::ended)
//! reads — D4 = (a): a device's blocked worker or exhausted budget ends the process by R1's
//! dead-worker path. A stop asked for (a device's Config changed, disable, delete, release; or the
//! process's stop) tells the worker to stop, and the keeper says when it has (`Event::Stopped`):
//! within [`DEVICE_STOP_BOUND`], or the watchdog's verdict that its thread is blocked — a death
//! either way. Nothing waits on the keeper but the stop's bounded wait (`done`).
//!
//! **Construction off to the side.** A factory runs on a thread of its own, then the driver's
//! `info()`, each within the host's `--factory-timeout`; a watcher task waits for each. One that
//! does not return is a death (D4 = a): a thread cannot be cancelled, and only the restart frees
//! it. A factory's failure, or its panic, is the instance's Config error (multi-device spec §3).

use std::collections::BTreeMap;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, Instant};

use lock_order::sync::Mutex;
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{Instrument, Span};

use crate::driver::{DeviceDriver, DriverInfo};
use crate::generations::DriverDescription;
use crate::host::{DeviceConfig, Factory, FactoryError};
use crate::link::{Abandoned, OpClock};
use crate::mailbox::Mailbox;
use crate::process::{supervise, Died};
use crate::reports::Reports;
use crate::worker::{self, Worker, WorkerEnd};

/// How long a device's stop asked for may take — its driver's disconnect (bounded at 2 s) and its
/// runtime's end — before its keeper declares the device dead. Past R1's watchdog bounds (a
/// blocked thread is a verdict within 5 s of its heartbeat's silence, and 2 s of a pause's grace),
/// so the watchdog's verdict comes first for a blocked thread; this is the backstop that makes the
/// stop's wait bounded whatever happens.
pub(crate) const DEVICE_STOP_BOUND: Duration = Duration::from_secs(10);

/// A worker started: its end, the clock and the blocking calls' count its watchdog reads, and its
/// operation bound.
pub(crate) struct Started {
    end: oneshot::Receiver<WorkerEnd>,
    clock: Arc<OpClock>,
    abandoned: Arc<Abandoned>,
    operation: Duration,
}

/// A driver built by the factory (or a fixed bridge's), its type erased: the session is not
/// generic over it.
pub(crate) trait Startable: Send + 'static {
    /// Pure driver information and policy, captured together exactly once.
    fn describe(&self) -> DriverDescription;
    /// Start R1's worker with this driver on a thread of its own, until `stop` turns true.
    fn start(
        self: Box<Self>,
        info: DriverInfo,
        instance: &str,
        mailbox: Arc<Mailbox>,
        reports: Arc<Reports>,
        stop: watch::Receiver<bool>,
        span: Span,
    ) -> std::io::Result<Started>;
}

/// A built driver of type `D`.
pub(crate) struct Built<D>(pub D);

impl<D: DeviceDriver> Startable for Built<D> {
    fn describe(&self) -> DriverDescription {
        DriverDescription::new(&self.0)
    }

    fn start(
        self: Box<Self>,
        info: DriverInfo,
        instance: &str,
        mailbox: Arc<Mailbox>,
        reports: Arc<Reports>,
        stop: watch::Receiver<bool>,
        span: Span,
    ) -> std::io::Result<Started> {
        let operation = info.timing.operation;
        let worker = Worker::new(self.0, info, instance, mailbox, reports);
        let (clock, abandoned) = (worker.clock(), worker.abandoned());
        let end = worker::spawn(worker, stop, span)?;
        Ok(Started { end, clock, abandoned, operation })
    }
}

/// Drop a driver nobody will start — a stale construction's, one a stop overtook — off the
/// session's thread: its `Drop` is the driver's, and may block.
pub(crate) fn drop_off_thread(driver: Box<dyn Startable>) {
    let _ = std::thread::Builder::new().name("bridge-drop".into()).spawn(move || drop(driver));
}

/// What a device's keeper, or a construction's watcher, tells the session.
type BuildResult = Result<(Box<dyn Startable>, DriverDescription), FactoryError>;

/// Completion of a device worker or factory.
pub(crate) enum Event {
    /// Device `id`'s worker has stopped, as asked.
    Stopped { id: u64 },
    /// Construction `id` of `instance` is done: the driver and its `info()`, or why not.
    Built { instance: String, id: u64, result: Box<BuildResult> },
}

/// A device's keeper, as the registry holds it: the stop request, and the keeper's end.
struct Keeper {
    ask: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

struct Keepers {
    /// The process is stopping: no device starts from now on.
    closed: bool,
    all: BTreeMap<u64, Keeper>,
}

/// Every running device's keeper, by device id: the session asks one to stop, the process's stop
/// asks them all.
pub(crate) struct Registry {
    keepers: Mutex<Keepers>,
}

impl Registry {
    pub(crate) fn new() -> Arc<Registry> {
        Arc::new(Registry { keepers: Mutex::new("devices-registry", Keepers { closed: false, all: BTreeMap::new() }) })
    }

    fn locked(&self) -> lock_order::sync::MutexGuard<'_, Keepers> {
        // WAIT: devices-registry
        self.keepers.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Keep device `id`'s keeper; `false` once the process is stopping.
    fn insert(&self, id: u64, keeper: Keeper) -> bool {
        let mut keepers = self.locked();
        if keepers.closed {
            return false;
        }
        keepers.all.insert(id, keeper);
        true
    }

    /// Ask device `id` to stop; whether it runs.
    pub(crate) fn stop(&self, id: u64) -> bool {
        match self.locked().all.get(&id) {
            Some(keeper) => {
                let _ = keeper.ask.send(true);
                true
            }
            None => false,
        }
    }

    fn remove(&self, id: u64) {
        self.locked().all.remove(&id);
    }

    /// The process's stop: no device starts from now on, every running one is asked to stop, and
    /// each keeper's end is handed back to wait for.
    pub(crate) fn close(&self) -> Vec<watch::Receiver<bool>> {
        let mut keepers = self.locked();
        keepers.closed = true;
        keepers
            .all
            .values()
            .map(|keeper| {
                let _ = keeper.ask.send(true);
                keeper.done.clone()
            })
            .collect()
    }
}

/// Wait until every keeper in `dones` has ended (or is gone). Each ends within its bound
/// ([`DEVICE_STOP_BOUND`], or its watchdog's verdict); callers bound the wait themselves.
pub(crate) async fn all_ended(dones: &mut [watch::Receiver<bool>]) {
    for done in dones {
        // WAIT: device-keeper
        let _ = done.wait_for(|ended| *ended).await;
    }
}

/// What starts devices and builds drivers: the registry, and where deaths and events go.
#[derive(Clone)]
pub(crate) struct Spawner {
    pub registry: Arc<Registry>,
    pub deaths: mpsc::UnboundedSender<Died>,
    pub events: mpsc::UnboundedSender<Event>,
}

impl Spawner {
    /// A spawner with a registry of its own, and the receiving ends of its deaths (the process's)
    /// and events (the session's).
    pub(crate) fn new() -> (Spawner, mpsc::UnboundedReceiver<Died>, mpsc::UnboundedReceiver<Event>) {
        let (deaths, deaths_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        (Spawner { registry: Registry::new(), deaths, events }, deaths_rx, events_rx)
    }

    /// Start device `id`'s worker with `built`, and its watchdog and keeper. `named`: the device is
    /// one of a host's — its deaths name its instance. A process already stopping starts the worker
    /// and asks it to stop at once.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        &self,
        id: u64,
        instance: &str,
        named: bool,
        built: Box<dyn Startable>,
        info: DriverInfo,
        mailbox: Arc<Mailbox>,
        reports: Arc<Reports>,
        span: Span,
    ) -> Result<(), String> {
        let (worker_stop, worker_stop_rx) = watch::channel(false);
        let started = built
            .start(info, instance, mailbox.clone(), reports.clone(), worker_stop_rx, span.clone())
            .map_err(|e| format!("the device worker's thread: {e}"))?;
        let Started { end, clock, abandoned, operation } = started;
        // The watchdog runs on its own, from now, whoever reads it (re-gate R1).
        let supervisor = tokio::spawn(supervise(clock, abandoned, operation * 2, reports, mailbox).instrument(span.clone()));
        let (ask, ask_rx) = watch::channel(false);
        let (done_tx, done) = watch::channel(false);
        let name = named.then(|| instance.to_owned());
        // Registered before the keeper runs, so a keeper that ends at once removes its own entry; a
        // process already stopping asks it to stop at once. The registry holds the stop request (a
        // request dropped reads as one asked).
        if !self.registry.insert(id, Keeper { ask: ask.clone(), done }) {
            let _ = ask.send(true);
        }
        tokio::spawn(keeper(id, name, end, supervisor, worker_stop, ask_rx, self.clone(), done_tx).instrument(span));
        Ok(())
    }

    /// Build a driver for `config` off to the side (multi-device spec §4.3): the factory, then the
    /// driver's `info()`, on a thread of their own, each within `bound`. The outcome reaches the
    /// session as `Event::Built`; one that does not return within its bound is a death (D4 = a).
    pub(crate) fn build(&self, factory: Factory, config: DeviceConfig, id: u64, bound: Duration, span: Span) {
        let instance = config.instance().to_owned();
        let (tx, mut rx) = mpsc::unbounded_channel::<Stage>();
        let thread = std::thread::Builder::new().name("bridge-factory".into()).spawn(move || {
            let built = std::panic::catch_unwind(AssertUnwindSafe(|| factory(&config)))
                .unwrap_or_else(|_| Err(FactoryError::Unusable("the factory panicked".into())));
            match built {
                Err(e) => {
                    let _ = tx.send(Stage::Failed(e));
                }
                Ok(driver) => {
                    let _ = tx.send(Stage::Built);
                    match std::panic::catch_unwind(AssertUnwindSafe(|| driver.describe())) {
                        Ok(info) => {
                            if let Err(error) = info.policy.validate(false) {
                                let _ = tx.send(Stage::Failed(FactoryError::Unusable(error)));
                                drop_off_thread(driver);
                            } else {
                                let _ = tx.send(Stage::Described(driver, info));
                            }
                        }
                        Err(_) => {
                            let _ = tx.send(Stage::Failed(FactoryError::Unusable("the driver's info() panicked".into())));
                        }
                    }
                }
            }
        });
        let spawner = self.clone();
        if let Err(e) = thread {
            let result = Err(FactoryError::Unusable(format!("the factory's thread could not start: {e}")));
            let _ = spawner.events.send(Event::Built { instance, id, result: Box::new(result) });
            return;
        }
        let watched = async move {
            let overdue = |what: &str| Died {
                part: "device",
                cause: format!("{instance}: {what} did not return within {} ms (--factory-timeout)", bound.as_millis()),
            };
            // WAIT: device-factory
            let result = match tokio::time::timeout(bound, rx.recv()).await {
                Err(_) => {
                    let _ = spawner.deaths.send(overdue("the factory"));
                    return;
                }
                Ok(Some(Stage::Failed(e))) => Err(e),
                Ok(Some(Stage::Described(driver, info))) => Ok((driver, info)),
                // WAIT: device-factory
                Ok(Some(Stage::Built)) => match tokio::time::timeout(bound, rx.recv()).await {
                    Err(_) => {
                        let _ = spawner.deaths.send(overdue("the driver's info()"));
                        return;
                    }
                    Ok(Some(Stage::Described(driver, info))) => Ok((driver, info)),
                    Ok(Some(Stage::Failed(e))) => Err(e),
                    Ok(Some(Stage::Built) | None) => Err(FactoryError::Unusable("the factory's thread ended without a word".into())),
                },
                Ok(None) => Err(FactoryError::Unusable("the factory's thread ended without a word".into())),
            };
            let _ = spawner.events.send(Event::Built { instance, id, result: Box::new(result) });
        };
        tokio::spawn(watched.instrument(span));
    }
}

/// How far a construction got, as its thread tells its watcher.
enum Stage {
    /// The factory returned a driver; its `info()` is next.
    Built,
    /// The driver and its `info()`.
    Described(Box<dyn Startable>, DriverDescription),
    /// The factory failed, or panicked (or `info()` did).
    Failed(FactoryError),
}

/// Sleep until `at`, or for good when there is none.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        // WAIT: device-keeper
        None => std::future::pending::<()>().await,
    }
}

/// A device's keeper (see the module documentation): until the worker has ended, been found
/// blocked, or overrun its stop.
#[allow(clippy::too_many_arguments)]
async fn keeper(
    id: u64,
    name: Option<String>,
    mut end: oneshot::Receiver<WorkerEnd>,
    mut supervisor: tokio::task::JoinHandle<(Died, bool)>,
    worker_stop: watch::Sender<bool>,
    mut ask: watch::Receiver<bool>,
    spawner: Spawner,
    done: watch::Sender<bool>,
) {
    let died = |cause: String| Died { part: "device", cause: name.as_ref().map_or(cause.clone(), |name| format!("{name}: {cause}")) };
    let (mut asked, mut watching) = (false, true);
    let mut deadline: Option<Instant> = None;
    loop {
        // WAIT: device-keeper
        tokio::select! {
            biased;
            verdict = &mut supervisor, if watching => {
                watching = false;
                match verdict {
                    Ok((verdict, blocked)) => {
                        let _ = spawner.deaths.send(died(verdict.cause));
                        // A blocked thread will not stop: not waited for. A full budget leaves a
                        // worker that still runs, stopped by the stop that follows.
                        if blocked {
                            break;
                        }
                    }
                    Err(e) if !e.is_cancelled() => {
                        let _ = spawner.deaths.send(died(format!("the watchdog ended: {e}")));
                    }
                    Err(_) => {}
                }
            }
            ended = &mut end => {
                if asked {
                    let _ = spawner.events.send(Event::Stopped { id });
                } else {
                    let _ = spawner.deaths.send(died(match ended {
                        Ok(WorkerEnd::Died(cause)) => cause,
                        Ok(WorkerEnd::Stopped) => "stopped".to_owned(),
                        Err(_) => "its thread ended without a word".to_owned(),
                    }));
                }
                break;
            }
            // A stop asked — or nobody left to ask, the process's parts dropped unstopped.
            _ = ask.wait_for(|stop| *stop), if !asked => {
                asked = true;
                let _ = worker_stop.send(true);
                deadline = Some(Instant::now() + DEVICE_STOP_BOUND);
            }
            () = sleep_until(deadline), if deadline.is_some() => {
                let _ = spawner.deaths.send(died(format!("the device worker did not stop within {} ms", DEVICE_STOP_BOUND.as_millis())));
                break;
            }
        }
    }
    supervisor.abort();
    spawner.registry.remove(id);
    let _ = done.send(true);
}

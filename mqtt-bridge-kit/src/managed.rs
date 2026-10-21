//! Process services independent of MQTT and device link lifetimes.
use crate::config::Settings;
use crate::devices::{drop_off_thread, Built, Registry};
use crate::generations::DriverDescription;
use crate::link::OpClock;
use crate::process::{Died, PauseGrace, SHUTDOWN_GRACE, WATCH_TICK};
use crate::{Bridge, DeviceDriver, Running};
use lock_order::sync::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};
use tokio::task::AbortHandle;
use tracing::{Instrument, Span};

/// Readiness and cooperative teardown bounds.
#[derive(Debug, Clone, Copy)]
pub struct ServiceTiming {
    /// Bound from spawning to validated readiness.
    pub startup: Duration,
    /// Cooperative stop before forced cancellation; the process ceiling wins.
    pub stop: Duration,
}
impl Default for ServiceTiming {
    fn default() -> Self {
        Self { startup: Duration::from_secs(10), stop: Duration::from_secs(2) }
    }
}
impl ServiceTiming {
    fn validate(self) -> Result<(), String> {
        if [self.startup, self.stop].iter().any(|d| d.is_zero() || *d > Duration::from_secs(604800)) {
            return Err("service bounds must be nonzero and at most one week".into());
        }
        Ok(())
    }
}
/// Construct without resources; own all service I/O and helpers inside run.
#[allow(async_fn_in_trait)]
pub trait ProcessService: Send + 'static {
    /// Proxy used by the ordinary device worker.
    type Driver: DeviceDriver;
    /// Prepare, publish readiness once, then serve until canceled. Futures need
    /// not be Send: they run on an isolated current-thread runtime.
    async fn run(self, context: ServiceContext<Self::Driver>) -> Result<(), String>;
}
#[derive(Default)]
struct Phase {
    ready: bool,
    stopping: bool,
    fatal: Option<Died>,
    shutdown_until: Option<tokio::time::Instant>,
}
struct Completed {
    result: Result<(), String>,
    at: tokio::time::Instant,
}
struct Attachment {
    registry: Arc<Registry>,
    shutdown: watch::Sender<bool>,
}
struct Control {
    phase: Mutex<Phase>,
    stop: watch::Sender<bool>,
    force: watch::Sender<bool>,
    fatal: watch::Sender<Option<Died>>,
    done: watch::Sender<Option<Completed>>,
    clock: Arc<OpClock>,
    attachment: OnceLock<Attachment>,
    timing: ServiceTiming,
}
impl Control {
    fn complete(&self, result: Result<(), String>) {
        // Serialize the completion fact with admission: no worker can start
        // after a completion already observed by the launch gate.
        // WAIT: managed-lifecycle
        let _phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        self.done.send_replace(Some(Completed { result, at: tokio::time::Instant::now() }));
    }
    fn stopping(&self, force: bool) {
        {
            // WAIT: managed-lifecycle
            let mut p = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            p.stopping = true;
            p.shutdown_until.get_or_insert_with(|| tokio::time::Instant::now() + SHUTDOWN_GRACE);
        }
        self.stop.send_replace(true);
        if force {
            self.force.send_replace(true);
        }
    }
    fn deadline(&self, requested: tokio::time::Instant) -> tokio::time::Instant {
        // WAIT: managed-lifecycle
        let mut p = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = requested.min(*p.shutdown_until.get_or_insert_with(|| tokio::time::Instant::now() + SHUTDOWN_GRACE));
        p.shutdown_until = Some(deadline);
        deadline
    }
    fn fatal(&self, cause: String) {
        let died = Died { part: "service", cause };
        {
            // WAIT: managed-lifecycle
            let mut p = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            if p.stopping || p.fatal.is_some() {
                return;
            }
            p.fatal = Some(died.clone());
            self.fatal.send_replace(Some(died));
        }
        self.stopping(true);
        if let Some(a) = self.attachment.get() {
            let dones = a.registry.close();
            let shutdown = a.shutdown.clone();
            let deadline = self.deadline(tokio::time::Instant::now() + SHUTDOWN_GRACE);
            tokio::spawn(async move {
                // WAIT: managed-fatal-stop
                let _ = tokio::time::timeout_at(deadline, crate::process::stop_devices_then_session(dones, shutdown)).await;
            });
        }
    }
    async fn stop_until(&self, deadline: tokio::time::Instant) -> bool {
        self.stopping(false);
        let deadline = self.deadline(deadline);
        let mut done = self.done.subscribe();
        let soft = (tokio::time::Instant::now() + self.timing.stop).min(deadline);
        // WAIT: managed-stop
        if tokio::time::timeout_at(soft, done.wait_for(Option::is_some)).await.is_ok() {
            return done.borrow().as_ref().is_some_and(|c| c.at <= deadline && c.result.is_ok());
        }
        self.stopping(true);
        // WAIT: managed-stop
        if tokio::time::timeout_at(deadline, done.wait_for(Option::is_some)).await.is_err() {
            return false;
        }
        let completed = done.borrow().as_ref().is_some_and(|c| c.at <= deadline && c.result.is_ok());
        completed
    }
}
/// One-way startup handoff and cancellation signal; owns no I/O.
pub struct ServiceContext<D: DeviceDriver> {
    ready: Option<oneshot::Sender<(D, DriverDescription)>>,
    control: Arc<Control>,
    settings: Settings,
    stopping: watch::Receiver<bool>,
    startup_until: Instant,
}
impl<D: DeviceDriver> ServiceContext<D> {
    /// Transfer a validated proxy once; never wait for MQTT, connect or reads.
    pub fn ready(&mut self, driver: D) -> Result<(), String> {
        {
            // WAIT: managed-lifecycle
            let p = self.control.phase.lock().unwrap_or_else(|p| p.into_inner());
            if p.stopping || p.fatal.is_some() || Instant::now() >= self.startup_until {
                return Err("service startup was canceled or expired".into());
            }
            if p.ready || self.ready.is_none() {
                return Err("service readiness may be published only once".into());
            }
        }
        let mut info = DriverDescription::new(&driver);
        info.info = crate::config::device_info(info.info, &self.settings.timing, self.settings.device_stable, &self.settings.mqtt)?;
        info.policy.validate(true)?;
        {
            // WAIT: managed-lifecycle
            let mut p = self.control.phase.lock().unwrap_or_else(|p| p.into_inner());
            if p.stopping || p.fatal.is_some() || Instant::now() >= self.startup_until {
                return Err("service startup was canceled or expired".into());
            }
            if p.ready || self.ready.is_none() {
                return Err("service readiness may be published only once".into());
            }
            p.ready = true;
            self.control.clock.beat();
            self.control.clock.set(None);
        }
        self.ready.take().unwrap().send((driver, info)).map_err(|_| "service startup receiver is gone".into())
    }
    /// Resolve on shutdown, including if the parent disappeared.
    pub async fn cancelled(&mut self) {
        // WAIT: managed-cancel
        let _ = self.stopping.wait_for(|stop| *stop).await;
    }
}
/// Keeps cleanup reachable after cancellation of the process launch future.
#[derive(Default)]
pub(crate) struct StartupSlot {
    control: OnceLock<Arc<Control>>,
}
impl StartupSlot {
    pub(crate) async fn stop(&self) -> bool {
        match self.control.get() {
            Some(c) => {
                // WAIT: managed-stop
                c.stop_until(tokio::time::Instant::now() + SHUTDOWN_GRACE).await
            }
            None => true,
        }
    }
}
pub(crate) struct Managed {
    control: Arc<Control>,
    fatal: watch::Receiver<Option<Died>>,
    watcher: AbortHandle,
}
impl Drop for Managed {
    fn drop(&mut self) {
        self.control.stopping(true);
        self.watcher.abort();
    }
}
impl Managed {
    pub(crate) fn admit<T>(&self, launch: impl FnOnce() -> Result<(T, Arc<Registry>, watch::Sender<bool>), String>) -> Result<T, String> {
        // WAIT: managed-lifecycle
        let p = self.control.phase.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(error) = self.failed_locked(&p) {
            return Err(error);
        }
        // Host launch only constructs runtime state and starts workers. It does
        // not await or call driver code on this thread (info was validated).
        let (running, registry, shutdown) = launch()?;
        let _ = self.control.attachment.set(Attachment { registry, shutdown });
        Ok(running)
    }
    pub(crate) fn shutdown_deadline(&self) -> tokio::time::Instant {
        self.control.deadline(tokio::time::Instant::now() + SHUTDOWN_GRACE)
    }
    pub(crate) async fn died(&mut self) -> Died {
        // WAIT: managed-fatal
        self.fatal.wait_for(Option::is_some).await.expect("fatal sender lives with owner").as_ref().unwrap().clone()
    }
    pub(crate) async fn stop_until(&self, deadline: tokio::time::Instant) -> bool {
        // WAIT: managed-stop
        self.control.stop_until(deadline).await
    }
    fn already_failed(&self) -> Option<String> {
        // WAIT: managed-lifecycle
        let p = self.control.phase.lock().unwrap_or_else(|p| p.into_inner());
        self.failed_locked(&p)
    }
    fn failed_locked(&self, p: &Phase) -> Option<String> {
        if let Some(d) = &p.fatal {
            return Some(d.cause.clone());
        }
        if p.stopping {
            return Some("service startup was canceled".into());
        }
        self.control.done.borrow().as_ref().map(|completed| match &completed.result {
            Ok(()) => "service returned before launch".into(),
            Err(e) => e.clone(),
        })
    }
}

type Readiness<D> = oneshot::Receiver<(D, DriverDescription)>;
fn spawn<S: ProcessService>(
    service: S,
    settings: Settings,
    timing: ServiceTiming,
    slot: &StartupSlot,
) -> Result<(Managed, Readiness<S::Driver>), String> {
    let (stop, stopping) = watch::channel(false);
    let (force, mut forcing) = watch::channel(false);
    let (fatal, fatal_rx) = watch::channel(None);
    let (done, _done) = watch::channel(None);
    let clock = Arc::new(OpClock::new());
    let startup_until = Instant::now() + timing.startup;
    clock.beat();
    clock.set(Some(startup_until));
    let control = Arc::new(Control {
        phase: Mutex::new("managed-lifecycle", Phase::default()),
        stop,
        force,
        fatal,
        done,
        clock,
        attachment: OnceLock::new(),
        timing,
    });
    slot.control.set(control.clone()).map_err(|_| "managed startup slot already used")?;
    let (ready, receive) = oneshot::channel();
    let ctx = ServiceContext { ready: Some(ready), control: control.clone(), settings, stopping, startup_until };
    let owner = control.clone();
    let span = Span::current();
    std::thread::Builder::new()
        .name("bridge-service".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let runtime =
                    tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| format!("service runtime: {e}"))?;
                let local = tokio::task::LocalSet::new();
                // WAIT: managed-thread
                let result = local.block_on(
                    &runtime,
                    async {
                        ctx.control.clock.beat();
                        tokio::task::spawn_local(crate::worker::heartbeat(ctx.control.clock.clone()));
                        // WAIT: managed-run
                        tokio::select! {biased;
                            _=forcing.wait_for(|force|*force)=>Ok(()),
                            result=service.run(ctx)=>result,
                        }
                    }
                    .instrument(span),
                );
                drop(local);
                // WAIT: managed-runtime
                // Runtime drop also confirms that its blocking helpers ended.
                // If one blocks, only this isolated OS thread is held: the
                // parent's absolute shutdown deadline reports abandonment.
                destroy_service_runtime(runtime);
                result
            }));
            let result = match result {
                Ok(result) => result,
                Err(payload) => Err(if let Some(s) = payload.downcast_ref::<String>() {
                    format!("service panicked: {s}")
                } else if let Some(s) = payload.downcast_ref::<&str>() {
                    format!("service panicked: {s}")
                } else {
                    "service panicked".into()
                }),
            };
            owner.complete(result);
        })
        .map_err(|e| {
            control.complete(Err(format!("service thread: {e}")));
            e.to_string()
        })?;
    let watching = control.clone();
    let task = tokio::spawn(async move {
        // WAIT: managed-watch
        watch(watching).await;
    });
    let watcher = task.abort_handle();
    drop(task);
    Ok((Managed { control, fatal: fatal_rx, watcher }, receive))
}
fn destroy_service_runtime(runtime: tokio::runtime::Runtime) {
    drop(runtime);
}
async fn watch(control: Arc<Control>) {
    let mut done = control.done.subscribe();
    let mut pause = PauseGrace::new(Instant::now());
    loop {
        // WAIT: managed-watch
        tokio::select! {
            result=done.wait_for(Option::is_some)=>{
                if let Ok(result)=result{
                    let cause=match &result.as_ref().unwrap().result{Ok(())=>"service returned unexpectedly".into(),Err(e)=>e.clone()};
                    drop(result);
                    control.fatal(cause);
                }
                return;
            },
            _=tokio::time::sleep(WATCH_TICK)=>{
                if !pause.judging(Instant::now()){continue;}
                if let Some(cause)=crate::process::blocked_reason(&control.clock,"service worker"){
                    control.fatal(cause);return;
                }
            },
        }
    }
}
impl Bridge {
    /// Start with a process-scoped service on the current runtime. No signals,
    /// logging or panic hook: this is the managed counterpart of `start`.
    pub async fn start_managed<S: ProcessService>(self, service: S) -> Result<Running, String> {
        self.start_managed_with(service, ServiceTiming::default()).await
    }
    /// Like `start_managed`, with explicit service timing bounds.
    pub async fn start_managed_with<S: ProcessService>(self, service: S, timing: ServiceTiming) -> Result<Running, String> {
        self.start_managed_in(service, timing, Arc::new(StartupSlot::default())).await
    }
    pub(crate) async fn start_managed_in<S: ProcessService>(
        self,
        service: S,
        timing: ServiceTiming,
        slot: Arc<StartupSlot>,
    ) -> Result<Running, String> {
        timing.validate()?;
        self.validate_managed_start()?;
        let (mut managed, ready) = spawn(service, self.settings().clone(), timing, &slot)?;
        // WAIT: managed-start
        let prepared = tokio::time::timeout(timing.startup, async {
            tokio::select! {biased;
                died=managed.died()=>Err(died.cause),
                ready=ready=>ready.map_err(|_|"service exited before readiness".into()),
            }
        })
        .await;
        let (driver, info) = match prepared {
            Ok(Ok(prepared)) => prepared,
            error => {
                // WAIT: managed-stop
                let closed = managed.stop_until(tokio::time::Instant::now() + SHUTDOWN_GRACE).await;
                let reason = match error {
                    Ok(Err(error)) => error,
                    _ => "service readiness deadline expired".into(),
                };
                return Err(format!("{reason}; teardown completed: {closed}"));
            }
        };
        if let Some(error) = managed.already_failed() {
            drop_off_thread(Box::new(Built(driver)));
            // WAIT: managed-stop
            let _ = managed.stop_until(tokio::time::Instant::now() + SHUTDOWN_GRACE).await;
            return Err(error);
        }
        match self.launch_managed(driver, info, managed) {
            Ok(running) => Ok(running),
            Err(error) => {
                // WAIT: managed-stop
                let _ = slot.stop().await;
                Err(error)
            }
        }
    }
}
#[cfg(test)]
mod tests;

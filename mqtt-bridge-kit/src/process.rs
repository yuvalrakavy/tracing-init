//! The bridge process (lifted from mqtt_ac's `main.rs` and `service.rs`; the fleet's F1, F3 and
//! no-hang 3b round 3).
//!
//! A [`Bridge`] is a facade over [`Host`] in its N = 1 mode (one fixed instance; see
//! [`crate::host`]), and this process is the same around either. [`Bridge::run`] is `main`'s whole
//! body:
//! - **Stop signals first:** SIGINT and SIGTERM are taken before anything else, so from then on
//!   either is a stop request — the bounded shutdown — whatever the start is doing. A failure to
//!   take them is WARN `signal_handler_unavailable`, and the bridge does not start; the logging
//!   start is waited for only within [`SHUTDOWN_GRACE`], to say so (past it, the failure status
//!   alone says it).
//! - **The logging start, raced against the stop:** tracing-init's start reads its configuration,
//!   opens today's log file and resolves the GELF host, synchronously; on a blocking thread, a stop
//!   ends the wait at once and the thread is abandoned.
//! - **No blocking prints:** nothing on a lifecycle path writes to stdout or stderr itself — a pipe
//!   nobody drains would hold the writer outside every bound. The command line's own output
//!   (`--help`, a usage error) is the exemption, before anything has started.
//! - **Panics:** the process's panic hook ([`install_panic_hook`]) logs ERROR `panic` through
//!   tracing, which is lossy, and hands the report for stderr to a thread of its own, abandoned
//!   after [`PANIC_REPORT_BOUND`]: nothing is written on the panicking thread, so a panic with a
//!   stderr nobody drains still reaches the `catch_unwind` or task end that turns it into a dead
//!   worker. The default hook wrote it there, and with the pipe full the death was never seen.
//! - **A bounded end:** the stop publishes `Active=false` and DISCONNECT and stops the device
//!   worker, within [`SHUTDOWN_GRACE`] (past it, WARN `shutdown_timeout`); the runtime, built
//!   explicitly, is shut down within [`RUNTIME_GRACE`] — dropped, it would wait without limit for a
//!   blocking thread (a stalled DNS lookup, a stalled logging start). `main` returns the `ExitCode`
//!   and never calls `process::exit`, which would skip every drop.
//! - **Dead workers:** none of the bridge's parts ends on its own; one that does (a driver's
//!   panic) is ERROR `worker_died`, the bounded shutdown, and a failure status, for the service
//!   manager to restart the bridge. So is a device worker whose thread is blocked — a synchronous
//!   call with no timeout, where its own timers cannot fire. A watchdog task, running from the
//!   start whoever reads it, reads the running operation's deadline (past it and
//!   [`BLOCKED_GRACE`]), the worker runtime's heartbeat when no operation runs (silent past
//!   [`IDLE_BOUND`]: a helper blocking the thread), and the blocking calls' cap (full for twice
//!   the operation bound): it then says the device is unreachable, refuses the momentary commands
//!   waiting, and ends the bridge. A thread cannot be cancelled; the restart is the recovery.

use std::collections::BTreeMap;
use std::future::Future;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tracing::{error, info, warn, Instrument, Span};

use crate::config::{Settings, TimingOverrides};
use crate::devices::{all_ended, Built, Registry};
use crate::driver::DeviceDriver;
use crate::host::Host;
use crate::link::{Abandoned, OpClock};
use crate::mailbox::Mailbox;
use crate::reports::{LinkStatus, Reports};
use crate::session::MqttTiming;
use crate::worker;

/// How long the bridge's parts get to stop before the process ends anyway.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long the stop waits for the device worker to stop — its driver's disconnect, bounded by
/// [`worker::DISCONNECT_BOUND`], and its runtime's end — before the session's flush goes ahead.
const WORKER_STOP_BOUND: Duration = Duration::from_millis(2500);
/// How long the runtime's threads get, once the bridge has stopped, before the process exits.
pub const RUNTIME_GRACE: Duration = Duration::from_secs(1);
/// Past its operation's deadline by this much, a device worker is blocked: its thread is in a
/// call its own timers cannot interrupt, and the watchdog declares it dead.
pub const BLOCKED_GRACE: Duration = Duration::from_secs(2);
/// How often the watchdog reads the operation's deadline and the heartbeat.
pub(crate) const WATCH_TICK: Duration = Duration::from_millis(100);
/// A watchdog tick later than asked by more than this is a pause of the whole process.
const PAUSE_SLACK: Duration = Duration::from_secs(1);
/// After a pause, how long the watchdog gives the worker to resume before it judges again.
const PAUSE_GRACE: Duration = Duration::from_secs(2);
/// With no operation running, a worker runtime silent this long has its thread blocked — a
/// helper's synchronous call — and is dead (re-gate R1).
pub const IDLE_BOUND: Duration = Duration::from_secs(5);
/// How long a panicking thread waits for its report to reach stderr before it goes on without.
pub const PANIC_REPORT_BOUND: Duration = Duration::from_millis(200);

/// Install the process's panic hook, once; later calls do nothing. [`Bridge::run`] installs it; a
/// process that runs bridges in-process ([`Bridge::start`]) installs it itself. Process-global, as
/// a panic hook is: one process may host several bridges.
///
/// The hook logs ERROR `panic` (the thread, where, and the payload) through tracing — lossy, so it
/// never waits — and writes the report to stderr on a thread of its own, waiting for it at most
/// [`PANIC_REPORT_BOUND`]: a stderr nobody drains holds that thread, never the panicking one.
pub fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    // A closure, so the hook's argument type is whichever this toolchain names it.
    INSTALLED.call_once(|| std::panic::set_hook(Box::new(|info| panic_hook(info.location(), info.payload()))));
}

fn panic_hook(location: Option<&std::panic::Location<'_>>, payload: &(dyn std::any::Any + Send)) {
    let thread = std::thread::current().name().unwrap_or("unnamed").to_owned();
    let location = location.map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column())).unwrap_or_default();
    let payload = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic without a message".to_owned());
    error!(kind = "panic", thread = %thread, location = %location, payload = %payload, "A thread panicked");
    if let Some(reports) = PANIC_REPORTS.get_or_init(|| PanicReports::start(|| Box::new(std::io::stderr()))) {
        reports.report(format!("thread '{thread}' panicked at {location}:\n{payload}\n"));
    }
}

/// The panic reports a panicking thread hands over, waiting for none (re-gate R8).
static PANIC_REPORTS: std::sync::OnceLock<Option<PanicReports>> = std::sync::OnceLock::new();

/// How many panic reports may wait for stderr; past it a report is dropped.
const PANIC_QUEUE: usize = 16;

/// The process's one panic reporter: a thread of its own that writes each report to stderr, fed by
/// a bounded queue. A stderr nobody drains holds that one thread, never a panicking one, and never
/// more threads: each panic used to start a writer, which a full stderr kept for good, so repeated
/// recoverable panics leaked a thread apiece.
struct PanicReports {
    queue: std::sync::mpsc::SyncSender<(String, std::sync::mpsc::SyncSender<()>)>,
}

impl PanicReports {
    /// The writer thread, writing to what `writer` opens; `None` if it cannot start.
    fn start(writer: impl FnOnce() -> Box<dyn std::io::Write + Send> + Send + 'static) -> Option<PanicReports> {
        let (queue, reports) = std::sync::mpsc::sync_channel::<(String, std::sync::mpsc::SyncSender<()>)>(PANIC_QUEUE);
        std::thread::Builder::new()
            .name("panic-report".into())
            .spawn(move || {
                let mut out = writer();
                // WAIT: panic-report
                while let Ok((report, written)) = reports.recv() {
                    let _ = out.write_all(report.as_bytes());
                    let _ = out.flush();
                    let _ = written.try_send(());
                }
            })
            .ok()?;
        Some(PanicReports { queue })
    }

    /// Hand `report` to the writer, and wait for it to be written at most [`PANIC_REPORT_BOUND`].
    /// A full queue drops it: lossy, never a wait past the bound.
    fn report(&self, report: String) {
        let (written, wait) = std::sync::mpsc::sync_channel::<()>(1);
        if self.queue.try_send((report, written)).is_ok() {
            // WAIT: panic-report
            let _ = wait.recv_timeout(PANIC_REPORT_BOUND);
        }
    }
}

/// A bridge, as `main` builds it: its family, its command line, and its driver's options, then
/// [`run`](Bridge::run) with a [`DeviceDriver`]. Every setter takes and returns `self`.
///
/// ```no_run
/// # use mqtt_bridge_kit::*;
/// # fn driver() -> impl DeviceDriver { struct D; impl DeviceDriver for D {
/// #     fn info(&self) -> DriverInfo { DriverInfo::default() }
/// #     async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> { Ok(()) }
/// #     async fn disconnect(&mut self) {}
/// #     async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> { Ok(vec![]) }
/// #     async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> { Ok(()) }
/// #     async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> { Ok(None) }
/// #     async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> { Ok(()) }
/// # } D }
/// fn main() -> std::process::ExitCode {
///     let bridge = Bridge::new("Aircondition").option("coolmaster", "the controller's address").require("coolmaster").from_cli();
///     // A value the driver cannot use is a usage error, said before anything starts.
///     let bridge = match bridge.value("coolmaster").map(str::parse::<std::net::SocketAddr>) {
///         Some(Err(e)) => bridge.usage_error(format!("`--coolmaster`: {e}")),
///         _ => bridge,
///     };
///     if let Some(code) = bridge.usage_exit() {
///         return code;
///     }
///     bridge.run(driver())
/// }
/// ```
///
/// Everything the command line sets can also be set in code (`root`, `instance`, `broker`,
/// `timing`, `device_stable`, `mqtt_timing`), which is how the conformance suite and in-process
/// users ([`start`](Bridge::start)) configure a bridge.
///
/// **One runtime.** A `Bridge` is the multi-device [`Host`]'s N = 1 mode (multi-device spec §4.1):
/// one fixed instance, named from `--instance`, served by the driver given to
/// [`run`](Bridge::run) — no Config, no `via` tag, its own `Active/{instance}`, and R1's CONNACK
/// duties and wire (see [`crate::host`]). The session, the publication rotation, the packet-id
/// budget and the shutdown flush are the host's own.
#[derive(Debug, Clone)]
pub struct Bridge(Host);

impl Bridge {
    /// Require a completed retained-snapshot replay and explicit engine install
    /// on every connection. Use with a managed service; legacy bootstrap is unchanged.
    pub fn recovery(self, options: crate::RecoveryOptions) -> Bridge {
        Bridge(self.0.recovery(options))
    }
    pub(crate) fn recovery_handle(&self) -> Option<crate::RecoveryHandle> {
        self.0.recovery_shared.as_ref().map(|recovery| recovery.handle())
    }
    pub(crate) fn prepare_recovery(&mut self) -> Result<(), String> {
        self.0.recovery_shared = self.0.recovery.clone().map(crate::recovery::RecoveryShared::new).transpose()?;
        Ok(())
    }
    pub(crate) fn validate_managed_start(&self) -> Result<(), String> {
        self.0.validate_managed_start()
    }
    pub(crate) fn launch_managed<D: DeviceDriver>(
        self,
        driver: D,
        info: crate::generations::DriverDescription,
        managed: crate::managed::Managed,
    ) -> Result<Running, String> {
        let mut pending: Option<Box<dyn crate::devices::Startable>> = Some(Box::new(Built(driver)));
        let launched = managed.admit(|| {
            let running = self.0.launch(Some((pending.take().unwrap(), Some(info))))?;
            let registry = running.registry.clone();
            let shutdown = running.shutdown.clone();
            Ok((running, registry, shutdown))
        });
        if let Some(driver) = pending {
            crate::devices::drop_off_thread(driver);
        }
        let mut running = launched?;
        running.managed = Some(managed);
        Ok(running)
    }
    /// A bridge of `family`, which is also the default MQTT root (`Aircondition`, `Lutron`, `IR`;
    /// `--root` overrides it). The program's name (its executable's file name) is the logging's
    /// application name and the usage text's, unless [`app_name`](Self::app_name) says otherwise.
    /// Nothing is read from the command line until [`from_cli`](Self::from_cli) or
    /// [`args`](Self::args).
    pub fn new(family: &str) -> Bridge {
        Bridge(Host::fixed(family))
    }

    /// Declare an option the driver takes: `--<name> <value>` (or `--<name>=<value>`; `name` is
    /// given without the dashes), listed in `--help` with `help`. Declare before
    /// [`from_cli`](Self::from_cli), or the parser rejects the flag as unknown; read the value
    /// with [`value`](Self::value) after it. An option is optional unless
    /// [`require`](Self::require)d, and takes a value always (there are no switches).
    pub fn option(self, name: &str, help: &str) -> Bridge {
        Bridge(self.0.option(name, help))
    }

    /// Make a declared option required: a command line without it is a usage error (exit 2, with
    /// the usage text). Before or after [`from_cli`](Self::from_cli). `--instance` and `--broker`
    /// are always required.
    pub fn require(self, name: &str) -> Bridge {
        Bridge(self.0.require(name))
    }

    /// A usage error the driver found in its options (an address it cannot use): `run`, or
    /// [`usage_exit`](Self::usage_exit), says it with the usage text and exits 2. A problem the
    /// command line already had stands.
    pub fn usage_error(self, message: impl Into<String>) -> Bridge {
        Bridge(self.0.usage_error(message))
    }

    /// The command line's own end, before any driver is built: `--help` (said on stdout, exit 0)
    /// or a usage error (said on stderr with the usage text, exit 2). `None` when the bridge is to
    /// run. `main` returns it: `if let Some(code) = bridge.usage_exit() { return code; }`.
    pub fn usage_exit(&self) -> Option<ExitCode> {
        self.0.usage_exit()
    }

    /// The logging's application name (the log file's prefix, GELF's `_app`); by default the
    /// program's name.
    pub fn app_name(self, name: &str) -> Bridge {
        Bridge(self.0.app_name(name))
    }

    /// The text of the bridge's retained `Version`.
    pub fn version(self, version: &str) -> Bridge {
        Bridge(self.0.version(version))
    }

    /// Opt into the request lifetime policy described by
    /// [`Host::request_lifetime_limit`]. Other bridges keep their existing
    /// deadline behavior unless they call this method.
    pub fn request_lifetime_limit(self, limit: Duration) -> Bridge {
        Bridge(self.0.request_lifetime_limit(limit))
    }

    /// Read the process's command line.
    pub fn from_cli(self) -> Bridge {
        Bridge(self.0.from_cli())
    }

    /// Read `args` as the command line (without the program's name).
    pub fn args(self, args: impl IntoIterator<Item = String>) -> Bridge {
        Bridge(self.0.args(args))
    }

    /// A declared option's value, if the command line gave one.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.0.value(name)
    }

    /// The driver's options the command line gave.
    pub fn values(&self) -> &BTreeMap<String, String> {
        self.0.values()
    }

    /// Set the root (in code; `--root` on the command line).
    pub fn root(self, root: &str) -> Bridge {
        Bridge(self.0.root(root))
    }

    /// Set the instance (in code; `--instance` on the command line).
    pub fn instance(self, instance: &str) -> Bridge {
        Bridge(self.0.instance(instance))
    }

    /// Set the broker, `host[:port]` (in code; `--broker` on the command line).
    pub fn broker(self, broker: &str) -> Bridge {
        Bridge(self.0.broker(broker))
    }

    /// Override the driver's timing (in code; one flag each on the command line).
    pub fn timing(self, overrides: TimingOverrides) -> Bridge {
        Bridge(self.0.timing(overrides))
    }

    /// A device link that lasts this long ends its outage (in code; `--device-stable` on the
    /// command line): overrides the driver's `Timing::stable`.
    pub fn device_stable(self, stable: Duration) -> Bridge {
        Bridge(self.0.device_stable(stable))
    }

    /// Set the MQTT side's timing (in code; `--mqtt-…` on the command line).
    pub fn mqtt_timing(self, timing: MqttTiming) -> Bridge {
        Bridge(self.0.mqtt_timing(timing))
    }

    /// The settings as given.
    pub fn settings(&self) -> &Settings {
        self.0.settings()
    }

    /// `main`'s whole body: `Bridge::new("Aircondition").from_cli().run(Coolmaster::new())`. It
    /// says `--help` or a usage error and returns its code (0, or 2) before anything else; otherwise
    /// it installs the panic hook, builds the tokio runtime, takes the stop signals, starts the
    /// logging, asks the driver for its `info()`, starts the bridge ([`start`](Self::start)) and
    /// blocks until SIGINT or SIGTERM (then the bounded shutdown, and `ExitCode::SUCCESS`) or until
    /// a part of the bridge dies (ERROR `worker_died`, the bounded shutdown, and
    /// `ExitCode::FAILURE`, for the service manager to restart). A start that fails (WARN
    /// `startup_failed`: a topic segment, the broker address, timings that cannot run) is a failure
    /// exit too. Return its value from `main`; never call `process::exit` after it. See the module
    /// documentation for each guarantee.
    pub fn run<D: DeviceDriver>(self, driver: D) -> ExitCode {
        if let Some(code) = self.usage_exit() {
            return code;
        }
        if self.0.recovery.is_some() {
            crate::devices::drop_off_thread(Box::new(Built(driver)));
            return self.usage_error("recovery requires run_managed or start_managed").usage_exit().unwrap_or(ExitCode::FAILURE);
        }
        let app = self.0.app();
        let log_config = self.0.settings().log_config.clone();
        // `info()` should be pure; one that is not must not hold up the stop: it runs on a
        // blocking thread raced against the stop signals (Codex, process.rs:251).
        let describe = move || {
            let info = crate::generations::DriverDescription::new(&driver);
            (driver, info)
        };
        run_with(
            app,
            log_config,
            describe,
            move |(driver, info)| async move { self.0.launch(Some((Box::new(Built(driver)), Some(info)))) },
            None,
        )
    }

    /// Run a fixed bridge with a process service under the ordinary signals,
    /// tracing-init and bounded shutdown policy. Construct the service without I/O.
    pub fn run_managed<S: crate::managed::ProcessService>(self, service: S) -> ExitCode {
        self.run_managed_with(service, crate::managed::ServiceTiming::default())
    }
    /// Like `run_managed`, with explicit service timing bounds.
    pub fn run_managed_with<S: crate::managed::ProcessService>(self, service: S, timing: crate::managed::ServiceTiming) -> ExitCode {
        if let Some(code) = self.usage_exit() {
            return code;
        }
        let app = self.0.app();
        let log_config = self.0.settings().log_config.clone();
        let slot = Arc::new(crate::managed::StartupSlot::default());
        let launch_slot = slot.clone();
        run_with(
            app,
            log_config,
            move || service,
            move |service| async move { self.start_managed_in(service, timing, launch_slot).await },
            Some(slot),
        )
    }

    /// Start the bridge on the current tokio runtime, in-process: the MQTT side as tasks, the
    /// device worker on a thread of its own, and the watchdog. No logging start, no signals and no
    /// panic hook — [`run`](Self::run) adds those; a process that runs bridges this way calls
    /// [`install_panic_hook`] itself (the conformance suite uses this directly, and keeps the test
    /// harness's own hook).
    ///
    /// A part's death — the watchdog's verdict on a blocked device worker included — reaches the
    /// caller only through [`Running::ended`]: an in-process user must poll it (in any slices it
    /// likes) and take the dead-part path itself, [`Running::stop`] and a restart. The watchdog runs
    /// whether or not it is polled, and says the device unreachable and refuses the waiting
    /// commands at once.
    pub fn start<D: DeviceDriver>(self, driver: D) -> Result<Running, String> {
        if self.0.recovery.is_some() {
            crate::devices::drop_off_thread(Box::new(Built(driver)));
            return Err("recovery requires run_managed or start_managed".into());
        }
        self.0.launch(Some((Box::new(Built(driver)), None)))
    }
}

/// [`Host::run`]'s process: a [`Bridge::run`]'s, with nothing to describe before the start (a
/// host's drivers are built per instance, each `info()` under the factory's bound).
pub(crate) fn run_process(
    app: String,
    log_config: Option<std::path::PathBuf>,
    launch: impl FnOnce() -> Result<Running, String>,
) -> ExitCode {
    run_with(app, log_config, || (), move |()| std::future::ready(launch()), None)
}

/// The process around a bridge or a host: the panic hook, an explicit runtime, `serve`, and the
/// runtime's bounded end. `describe` runs on a blocking thread raced against the stop signals,
/// then `launch` starts the parts.
fn run_with<P: Send + 'static, F: Future<Output = Result<Running, String>>>(
    app: String,
    log_config: Option<std::path::PathBuf>,
    describe: impl FnOnce() -> P + Send + 'static,
    launch: impl FnOnce(P) -> F,
    startup: Option<Arc<crate::managed::StartupSlot>>,
) -> ExitCode {
    install_panic_hook();
    // No runtime, no bridge: said by the status alone (there is no log yet).
    let Ok(runtime) = tokio::runtime::Builder::new_multi_thread().enable_all().build() else {
        return ExitCode::FAILURE;
    };
    // WAIT: runtime
    let exit = runtime.block_on(serve_with(app, log_config, describe, launch, startup));
    // Shut down within a bound, never dropped: dropping waits for every thread still in a
    // synchronous call — a DNS lookup of the broker's name (tokio and rumqttc resolve names on
    // blocking threads), a stalled logging start — without limit.
    runtime.shutdown_timeout(RUNTIME_GRACE);
    exit
}

/// From the stop signals to the bounded shutdown. tracing-init's guard lives here and is
/// dropped when this returns, inside the runtime.
async fn serve_with<P: Send + 'static, F: Future<Output = Result<Running, String>>>(
    app: String,
    log_config: Option<std::path::PathBuf>,
    describe: impl FnOnce() -> P + Send + 'static,
    launch: impl FnOnce(P) -> F,
    startup: Option<Arc<crate::managed::StartupSlot>>,
) -> ExitCode {
    let mut signals = StopSignals::register();
    let starting = tokio::task::spawn_blocking(move || crate::logging::start(&app, log_config.as_deref()));
    let logging = match &mut signals {
        Ok(signals) => {
            // WAIT: logging-start
            tokio::select! {
                started = starting => started,
                _ = signals.next() => {
                    // Stopped before there is a log to say so in.
                    return ExitCode::SUCCESS;
                }
            }
        }
        Err(_) => {
            // The bridge will not start; the wait is only to say why, so it is bounded.
            // WAIT: logging-start
            match tokio::time::timeout(SHUTDOWN_GRACE, starting).await {
                Ok(started) => started,
                // No log to say it in: the status says it.
                Err(_) => return ExitCode::FAILURE,
            }
        }
    };
    // A start that failed said so on its own thread; one that panicked, through the hook.
    let _logging = logging.ok().flatten();
    if let Some(guard) = &_logging {
        info!(logging = %guard, "Logging started");
    }
    let mut signals = match signals {
        Ok(signals) => signals,
        Err(e) => {
            warn!(kind = "signal_handler_unavailable", error = %e, "The stop signals cannot be handled; not starting");
            return ExitCode::FAILURE;
        }
    };
    // A bridge's `info()`, on a blocking thread raced against the stop signals: a stop abandons it
    // within the runtime's grace (Codex, process.rs:251).
    let describing = tokio::task::spawn_blocking(describe);
    // WAIT: driver-info
    let described = tokio::select! {
        described = describing => match described {
            Ok(described) => described,
            // A panic, said through the hook.
            Err(e) => {
                warn!(kind = "startup_failed", error = %e, "The driver could not describe itself");
                return ExitCode::FAILURE;
            }
        },
        _ = signals.next() => {
            info!("Stopped before the driver described itself");
            return ExitCode::SUCCESS;
        }
    };
    let mut launching = Box::pin(launch(described));
    // WAIT: process-launch
    let launched = tokio::select! {
        biased;
        _=signals.next()=>{
            drop(launching);
            // WAIT: managed-stop
            return if let Some(startup)=startup{if startup.stop().await{ExitCode::SUCCESS}else{ExitCode::FAILURE}}else{ExitCode::SUCCESS};
        },
        result=&mut launching=>result,
    };
    drop(launching);
    let running = match launched {
        Ok(running) => running,
        Err(e) => {
            if let Some(startup) = startup {
                // WAIT: managed-stop
                let _ = startup.stop().await;
            }
            warn!(kind = "startup_failed", error = %e, "The bridge could not start");
            return ExitCode::FAILURE;
        }
    };
    let span = running.span.clone();
    match serve(running, signals.next()).instrument(span).await {
        Ending::Stopped => ExitCode::SUCCESS,
        Ending::WorkerDied => ExitCode::FAILURE,
    }
}

/// A running bridge or host (see [`Bridge::start`], [`Host::start`](crate::Host::start)).
pub struct Running {
    pub(crate) managed: Option<crate::managed::Managed>,
    tasks: JoinSet<()>,
    shutdown: watch::Sender<bool>,
    span: Span,
    /// Every running device's keeper: the stop asks them all.
    registry: Arc<Registry>,
    /// The devices' deaths — a watchdog's verdict, a worker's end nobody asked for, a factory that
    /// did not return — each kept until read.
    deaths: mpsc::UnboundedReceiver<Died>,
}

impl Running {
    /// The parts of a running bridge or host, for [`Host::start`](crate::Host::start).
    pub(crate) fn new(
        tasks: JoinSet<()>,
        shutdown: watch::Sender<bool>,
        span: Span,
        registry: Arc<Registry>,
        deaths: mpsc::UnboundedReceiver<Died>,
    ) -> Running {
        Running { tasks, shutdown, span, registry, deaths, managed: None }
    }
}

/// Dropped unstopped — the runtime living on — a bridge or host closes admission and asks every
/// device to stop, as R1's `Running` did when its worker's stop sender went with it (R2 gate, Codex
/// 8): each keeper holds the registry, so without this its stop request never closes, and a healthy
/// worker would keep its device's link and poll on for nobody. After [`Running::stop`] it does
/// nothing more.
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.registry.close();
    }
}

/// The watchdog, until the device worker is dead: an operation past its deadline by
/// [`BLOCKED_GRACE`]; with none running, a heartbeat silent past [`IDLE_BOUND`]; or a blocking
/// budget — the operations' or the helpers' — full for `cap_bound`. A worker whose thread is
/// blocked will not answer: the device is said unreachable and what waits refused — at once,
/// whether or not anyone reads the verdict yet. A full budget leaves a worker that still runs: it
/// is stopped, and what waits refused, by the stop that follows (re-gate 4 minor 8: refused here,
/// the mailbox was marked disconnected under a live worker). Returns the verdict, and whether the
/// worker's thread is blocked.
pub(crate) async fn supervise(
    clock: Arc<OpClock>,
    abandoned: Arc<Abandoned>,
    cap_bound: Duration,
    reports: Arc<Reports>,
    mailbox: Arc<Mailbox>,
) -> (Died, bool) {
    let mut pause = PauseGrace::new(std::time::Instant::now());
    let cause = loop {
        tokio::time::sleep(WATCH_TICK).await;
        if !pause.judging(std::time::Instant::now()) {
            continue;
        }
        if let Some(cause) = blocked_reason(&clock, "device worker") {
            break (cause, true);
        }
        if let Some((full, whose)) = abandoned.full_for().filter(|(full, _)| *full > cap_bound) {
            break (
                format!(
                    "the device worker's {whose} blocking calls have filled their cap for {} ms: threads stuck in synchronous calls",
                    full.as_millis()
                ),
                false,
            );
        }
    };
    let (cause, blocked) = cause;
    if blocked {
        reports.status(LinkStatus::Unreachable);
        worker::report_refused(&reports, mailbox.disconnected(), worker::LINK_LOST_BEFORE);
    }
    (Died { part: "device", cause }, blocked)
}

/// Shared watchdog predicate; each component keeps its own failure effects.
pub(crate) fn blocked_reason(clock: &OpClock, label: &str) -> Option<String> {
    if let Some(deadline) = clock.deadline() {
        let over = std::time::Instant::now().saturating_duration_since(deadline);
        if over > BLOCKED_GRACE {
            return Some(format!("the {label} blocked its thread {} ms past its operation's deadline", over.as_millis()));
        }
    } else if let Some(quiet) = clock.since_beat().filter(|q| *q > IDLE_BOUND) {
        return Some(format!("the {label}'s thread was blocked for {} ms with no operation running", quiet.as_millis()));
    }
    None
}

/// Shared order: close device admission, allow bounded disconnect, then flush MQTT.
pub(crate) async fn stop_devices_then_session(mut dones: Vec<watch::Receiver<bool>>, shutdown: watch::Sender<bool>) {
    // WAIT: shutdown
    let workers_ended = tokio::time::timeout(WORKER_STOP_BOUND, all_ended(&mut dones)).await.is_ok();
    let _ = shutdown.send(true);
    if !workers_ended {
        all_ended(&mut dones).await;
    }
}

/// Whether a watchdog tick may judge. A tick that came far later than asked — the process was
/// paused (SIGSTOP, a VM's pause) — starts a grace: the worker was paused too, and its heartbeat
/// and timers resume with it, so what the clocks say across the pause is no verdict (re-gate 3 S6).
pub(crate) struct PauseGrace {
    last_tick: std::time::Instant,
    grace_until: Option<std::time::Instant>,
}

impl PauseGrace {
    pub(crate) fn new(now: std::time::Instant) -> Self {
        PauseGrace { last_tick: now, grace_until: None }
    }

    /// The tick at `now`: may it judge the worker?
    pub(crate) fn judging(&mut self, now: std::time::Instant) -> bool {
        let gap = now.saturating_duration_since(self.last_tick);
        self.last_tick = now;
        if gap > WATCH_TICK + PAUSE_SLACK {
            self.grace_until = Some(now + PAUSE_GRACE);
            return false;
        }
        !self.grace_until.is_some_and(|until| now < until)
    }
}

/// A part of the bridge that ended on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Died {
    /// Which part ended: `"mqtt"`, `"device"`, or a managed process `"service"`.
    pub part: &'static str,
    /// Why it ended.
    pub cause: String,
}

impl Running {
    /// The first part of the bridge to end — none ends on its own, so it is a death: the MQTT side
    /// (`mqtt`) or a device (`device`) — its worker's end, a worker blocked past its operation's
    /// deadline (then the device is said unreachable and the momentary commands waiting are
    /// refused first), a blocking budget full for good, and for a host a device's factory or
    /// `info()` that did not return in time (multi-device spec §4.1, D4 = a: each ends the
    /// process). Each death is kept until read. Cancellation-safe.
    pub async fn ended(&mut self) -> Died {
        // WAIT: bridge-parts
        tokio::select! {
            biased;
            died = async {
                match self.managed.as_mut() {
                    Some(service) => service.died().await,
                    None => {
                        // WAIT: bridge-parts
                        std::future::pending().await
                    }
                }
            } => died,
            Some(died) = self.deaths.recv() => died,
            Some(joined) = self.tasks.join_next() => Died { part: "mqtt", cause: match joined {
                Ok(()) => "returned".to_owned(),
                Err(e) => e.to_string(),
            }},
        }
    }

    /// The bounded shutdown: every device worker stopped and its driver disconnected, then the
    /// commands still waiting refused, the reports pending published, `Active=false` and
    /// DISCONNECT, within [`SHUTDOWN_GRACE`]. Past it, one WARN `shutdown_timeout`, and what is
    /// left is abandoned. Returns whether it ended in time.
    pub async fn stop(mut self) -> bool {
        let span = self.span.clone();
        async move {
            // The workers first: each takes nothing more, and what it runs is cut off. Then the
            // session: its flush refuses what the workers left — the command each was cut off from
            // answering included — on Error and on its response topic, before its last words
            // (re-gate R15). In that order, nothing a worker answers races the flush's refusals
            // (re-gate 4 F3, minor 2). Each device's keeper watches its watchdog while it waits: a
            // verdict that a worker's thread is blocked, reached before the stop or during its wait,
            // ends that keeper's wait at once (re-gate 4 minor 4; re-gate 5) — a blocked worker
            // will not stop, and is not waited for. No device starts once the stop has begun.
            let deadline =
                self.managed.as_ref().map_or_else(|| tokio::time::Instant::now() + SHUTDOWN_GRACE, |service| service.shutdown_deadline());
            let dones = self.registry.close();
            let managed = self.managed.take();
            let shutdown = &self.shutdown;
            let tasks = &mut self.tasks;
            let all = async move {
                stop_devices_then_session(dones, shutdown.clone()).await;
                // WAIT: shutdown
                while tasks.join_next().await.is_some() {}
            };
            let service_stop = async {
                match managed.as_ref() {
                    Some(service) => {
                        // WAIT: managed-stop
                        service.stop_until(deadline).await
                    }
                    None => true,
                }
            };
            let mut service_ended = false;
            let ended = stop_before(deadline, SHUTDOWN_GRACE, async {
                // WAIT: shutdown
                let (_, service) = tokio::join!(all, service_stop);
                service_ended = service;
            })
            .await
                && service_ended;
            if !ended {
                self.tasks.abort_all();
            }
            info!(in_time = ended, "Bridge stopped");
            ended
        }
        .instrument(span)
        .await
    }
}

/// `stop`, for at most `grace`; past it one WARN `shutdown_timeout`. Returns whether it ended.
#[cfg(test)]
pub(crate) async fn stop_within<T>(grace: Duration, stop: impl Future<Output = T>) -> bool {
    stop_before(tokio::time::Instant::now() + grace, grace, stop).await
}
async fn stop_before<T>(deadline: tokio::time::Instant, grace: Duration, stop: impl Future<Output = T>) -> bool {
    // Tokio polls the operation first; explicitly reject late ready completion.
    // WAIT: shutdown
    let ended = tokio::time::Instant::now() < deadline
        && tokio::time::timeout_at(deadline, stop).await.is_ok()
        && tokio::time::Instant::now() <= deadline;
    if !ended {
        warn!(
            kind = "shutdown_timeout",
            grace_ms = grace.as_millis() as u64,
            "The bridge did not stop within its shutdown bound; exiting anyway"
        );
    }
    ended
}

/// How the bridge ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    Stopped,
    WorkerDied,
}

/// The running bridge, until a stop signal or the death of one of its parts, then its bounded
/// shutdown.
pub(crate) async fn serve(mut running: Running, stop: impl Future<Output = &'static str>) -> Ending {
    let managed = running.managed.is_some();
    // WAIT: bridge-end
    let ending = tokio::select! {
        signal = stop => {
            info!(signal, "Stop requested; shutting down");
            Ending::Stopped
        }
        died = running.ended() => {
            error!(kind = "worker_died", worker = died.part, error = %died.cause, "A part of the bridge ended; shutting down, for the service manager to restart it");
            Ending::WorkerDied
        }
    };
    // WAIT: shutdown
    let stopped = running.stop().await;
    if managed && !stopped {
        Ending::WorkerDied
    } else {
        ending
    }
}

/// The test seam's environment variable (with the `test-seams` feature only): set, the stop signals cannot be
/// taken (`StopSignals::register` fails), as when the process may not install handlers.
pub const TEST_NO_SIGNALS: &str = "MQTT_BRIDGE_TEST_NO_SIGNALS";

/// What stops the bridge: SIGINT (ctrl-c) and SIGTERM (`kill`, systemd), registered first thing.
struct StopSignals {
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
}

impl StopSignals {
    #[cfg(unix)]
    fn register() -> std::io::Result<StopSignals> {
        use tokio::signal::unix::{signal, SignalKind};
        #[cfg(feature = "test-seams")]
        if std::env::var_os(TEST_NO_SIGNALS).is_some() {
            return Err(std::io::Error::other("the test seam refused the stop signals"));
        }
        Ok(StopSignals { interrupt: signal(SignalKind::interrupt())?, terminate: signal(SignalKind::terminate())? })
    }

    #[cfg(not(unix))]
    fn register() -> std::io::Result<StopSignals> {
        Ok(StopSignals {})
    }

    /// The next stop signal to arrive, by name. Cancellation-safe.
    #[cfg(unix)]
    async fn next(&mut self) -> &'static str {
        // WAIT: stop-signal
        tokio::select! {
            _ = self.interrupt.recv() => "SIGINT",
            _ = self.terminate.recv() => "SIGTERM",
        }
    }

    #[cfg(not(unix))]
    async fn next(&mut self) -> &'static str {
        // WAIT: stop-signal
        let _ = tokio::signal::ctrl_c().await;
        "ctrl-c"
    }
}

#[cfg(test)]
mod tests;

//! The bridge process (lifted from mqtt_ac's `main.rs` and `service.rs`; the fleet's F1, F3 and
//! no-hang 3b round 3).
//!
//! [`Bridge::run`] is `main`'s whole body:
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
//!   call with no timeout, where its own timers cannot fire: a watchdog here reads the running
//!   operation's deadline, and past it and [`BLOCKED_GRACE`] says the device is unreachable,
//!   refuses the momentary commands waiting, and ends the bridge. A thread cannot be cancelled;
//!   the restart is the recovery.

use std::collections::BTreeMap;
use std::future::Future;
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tracing::{error, info, warn, Instrument, Span};

use crate::config::{self, Problem, Settings, TimingOverrides};
use crate::driver::{DeviceDriver, DriverInfo};
use crate::link::OpClock;
use crate::mailbox::Mailbox;
use crate::pump::BrokerOutage;
use crate::reports::{LinkStatus, Reports};
use crate::session::{self, MqttTiming};
use crate::topics::Topics;
use crate::worker::{self, Worker, WorkerEnd};

/// How long the bridge's parts get to stop before the process ends anyway.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
/// How long the runtime's threads get, once the bridge has stopped, before the process exits.
pub const RUNTIME_GRACE: Duration = Duration::from_secs(1);
/// Past its operation's deadline by this much, a device worker is blocked: its thread is in a
/// call its own timers cannot interrupt, and the watchdog declares it dead.
pub const BLOCKED_GRACE: Duration = Duration::from_secs(2);
/// How often the watchdog reads the operation's deadline.
const WATCH_TICK: Duration = Duration::from_millis(100);
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
    let report = format!("thread '{thread}' panicked at {location}:\n{payload}\n");
    let (written, wait) = std::sync::mpsc::channel::<()>();
    let reporter = std::thread::Builder::new().name("panic-report".into()).spawn(move || {
        use std::io::Write;
        let _ = std::io::stderr().write_all(report.as_bytes());
        let _ = written.send(());
    });
    if reporter.is_ok() {
        // WAIT: panic-report
        let _ = wait.recv_timeout(PANIC_REPORT_BOUND);
    }
}

/// A bridge: its family, its command line, and its driver's options. See the crate documentation.
#[derive(Debug, Clone)]
pub struct Bridge {
    family: String,
    program: String,
    app_name: Option<String>,
    version: Option<String>,
    declared: Vec<(String, String)>,
    /// The declared options the command line must give.
    required: Vec<String>,
    /// Whether a command line has been read.
    parsed: bool,
    settings: Settings,
    problem: Option<Problem>,
}

impl Bridge {
    /// A bridge of `family`: the default root (`Aircondition`, `Lutron`, `IR`).
    pub fn new(family: &str) -> Bridge {
        let program = std::env::args_os()
            .next()
            .and_then(|a| std::path::Path::new(&a).file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| family.to_lowercase());
        Bridge {
            family: family.to_owned(),
            program,
            app_name: None,
            version: None,
            declared: Vec::new(),
            required: Vec::new(),
            parsed: false,
            settings: Settings::new(family),
            problem: None,
        }
    }

    /// Declare an option the driver takes (`--<name> <value>`), with its help; read it with
    /// [`value`](Self::value) after [`from_cli`](Self::from_cli).
    pub fn option(mut self, name: &str, help: &str) -> Bridge {
        self.declared.push((name.to_owned(), help.to_owned()));
        self
    }

    /// Make a declared option required: a command line without it is a usage error (exit 2, with
    /// the usage text). Before or after [`from_cli`](Self::from_cli).
    pub fn require(mut self, name: &str) -> Bridge {
        self.required.push(name.to_owned());
        if self.parsed {
            self.check_required();
        }
        self
    }

    /// A usage error the driver found in its options (an address it cannot use): `run`, or
    /// [`usage_exit`](Self::usage_exit), says it with the usage text and exits 2. A problem the
    /// command line already had stands.
    pub fn usage_error(mut self, message: impl Into<String>) -> Bridge {
        if self.problem.is_none() {
            self.problem = Some(Problem::Usage(message.into()));
        }
        self
    }

    /// The command line's own end, before any driver is built: `--help` (said on stdout, exit 0)
    /// or a usage error (said on stderr with the usage text, exit 2). `None` when the bridge is to
    /// run. `main` returns it: `if let Some(code) = bridge.usage_exit() { return code; }`.
    pub fn usage_exit(&self) -> Option<ExitCode> {
        match &self.problem {
            Some(Problem::Help) => {
                // The command line's own output, before anything has started: exempt.
                print!("{}", config::usage(&self.program, &self.family, &self.declared));
                Some(ExitCode::SUCCESS)
            }
            Some(Problem::Usage(message)) => {
                eprint!("{}: {message}\n\n{}", self.program, config::usage(&self.program, &self.family, &self.declared));
                Some(ExitCode::from(2))
            }
            None => None,
        }
    }

    fn check_required(&mut self) {
        if self.problem.is_some() {
            return;
        }
        if let Some(missing) = self.required.iter().find(|name| !self.settings.options.contains_key(*name)) {
            self.problem = Some(Problem::Usage(format!("`--{missing}` is required")));
        }
    }

    /// The logging's application name (the log file's prefix, GELF's `_app`); by default the
    /// program's name.
    pub fn app_name(mut self, name: &str) -> Bridge {
        self.app_name = Some(name.to_owned());
        self
    }

    /// The text of the bridge's retained `Version`.
    pub fn version(mut self, version: &str) -> Bridge {
        self.version = Some(version.to_owned());
        self
    }

    /// Read the process's command line.
    pub fn from_cli(self) -> Bridge {
        self.args(std::env::args().skip(1))
    }

    /// Read `args` as the command line (without the program's name).
    pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Bridge {
        self.parsed = true;
        if let Err(problem) = config::parse(args, &mut self.settings, &self.declared) {
            self.problem = Some(problem);
        }
        self.check_required();
        self
    }

    /// A declared option's value, if the command line gave one.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.settings.options.get(name).map(String::as_str)
    }

    /// The driver's options the command line gave.
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.settings.options
    }

    /// Set the root (in code; `--root` on the command line).
    pub fn root(mut self, root: &str) -> Bridge {
        self.settings.root = root.to_owned();
        self
    }

    /// Set the instance (in code; `--instance` on the command line).
    pub fn instance(mut self, instance: &str) -> Bridge {
        self.settings.instance = Some(instance.to_owned());
        self
    }

    /// Set the broker, `host[:port]` (in code; `--broker` on the command line).
    pub fn broker(mut self, broker: &str) -> Bridge {
        self.settings.broker = Some(broker.to_owned());
        self
    }

    /// Override the driver's timing (in code; one flag each on the command line).
    pub fn timing(mut self, overrides: TimingOverrides) -> Bridge {
        self.settings.timing = overrides;
        self
    }

    /// Set the MQTT side's timing (in code; `--mqtt-…` on the command line).
    pub fn mqtt_timing(mut self, timing: MqttTiming) -> Bridge {
        self.settings.mqtt = timing;
        self
    }

    /// The settings as given.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn app(&self) -> String {
        self.app_name.clone().unwrap_or_else(|| self.program.clone())
    }

    /// `main`'s whole body: `Bridge::new("Aircondition").from_cli().run(Coolmaster::new())`. See the
    /// module documentation.
    pub fn run<D: DeviceDriver>(self, driver: D) -> ExitCode {
        if let Some(code) = self.usage_exit() {
            return code;
        }
        install_panic_hook();
        // No runtime, no bridge: said by the status alone (there is no log yet).
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread().enable_all().build() else {
            return ExitCode::FAILURE;
        };
        // WAIT: runtime
        let exit = runtime.block_on(self.serve(driver));
        // Shut down within a bound, never dropped: dropping waits for every thread still in a
        // synchronous call — a DNS lookup of the broker's name (tokio and rumqttc resolve names on
        // blocking threads), a stalled logging start — without limit.
        runtime.shutdown_timeout(RUNTIME_GRACE);
        exit
    }

    /// From the stop signals to the bounded shutdown. tracing-init's guard lives here and is
    /// dropped when this returns, inside the runtime.
    async fn serve<D: DeviceDriver>(self, driver: D) -> ExitCode {
        let mut signals = StopSignals::register();
        let app = self.app();
        let log_config = self.settings.log_config.clone();
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
        let running = match self.start(driver) {
            Ok(running) => running,
            Err(e) => {
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

    /// Start the bridge on the current tokio runtime, in-process: the MQTT side as tasks, the
    /// device worker on a thread of its own. No logging start, no signals and no panic hook —
    /// [`run`](Self::run) adds those; a process that runs bridges this way calls
    /// [`install_panic_hook`] itself (the conformance suite uses this directly, and keeps the test
    /// harness's own hook).
    pub fn start<D: DeviceDriver>(self, driver: D) -> Result<Running, String> {
        if let Some(problem) = &self.problem {
            return Err(match problem {
                Problem::Help => "--help".to_owned(),
                Problem::Usage(m) => m.clone(),
            });
        }
        let instance = self.settings.instance.clone().ok_or("no instance")?;
        let topics = Topics::new(&self.settings.root, &instance)?;
        let (broker_host, broker_port) = config::broker_host_port(self.settings.broker.as_deref().ok_or("no broker")?)?;
        let span = tracing::info_span!("bridge", root = %topics.root(), instance = %instance);
        let info: DriverInfo = driver.info();
        let info = DriverInfo { timing: self.settings.timing.apply(info.timing), ..info };
        config::check_timing(&info.timing, &self.settings.mqtt)?;
        let mailbox = Mailbox::new();
        let reports = Reports::new();
        let shared = Arc::new(session::Shared {
            topics,
            broker_host,
            broker_port,
            timing: self.settings.mqtt,
            version: self.version.clone().unwrap_or_else(|| format!("{} (mqtt-bridge-kit {})", self.app(), env!("CARGO_PKG_VERSION"))),
            commands: info.commands.clone(),
            mailbox: mailbox.clone(),
            reports: reports.clone(),
            outage: Arc::new(BrokerOutage::new(self.settings.mqtt.warn_after, self.settings.mqtt.stable_after)),
            read_back_requested: AtomicBool::new(false),
        });
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (worker_stop, worker_stop_rx) = watch::channel(false);
        let worker = Worker::new(driver, info, &instance, mailbox.clone(), reports.clone());
        let clock = worker.clock();
        let worker = worker::spawn(worker, worker_stop_rx, span.clone()).map_err(|e| format!("the device worker's thread: {e}"))?;
        let mut tasks = JoinSet::new();
        let _entered = span.enter();
        session::spawn(shared, shutdown_rx, &mut tasks);
        drop(_entered);
        Ok(Running { tasks, worker: Some(worker), shutdown, worker_stop, span, clock, watching: true, reports, mailbox })
    }
}

/// A running bridge (see [`Bridge::start`]).
pub struct Running {
    tasks: JoinSet<()>,
    worker: Option<oneshot::Receiver<WorkerEnd>>,
    shutdown: watch::Sender<bool>,
    worker_stop: watch::Sender<bool>,
    span: Span,
    /// The device worker's operation clock, and whether the watchdog still reads it (it fires
    /// once).
    clock: Arc<OpClock>,
    watching: bool,
    reports: Arc<Reports>,
    mailbox: Arc<Mailbox>,
}

/// Until the device worker's running operation is past its deadline by [`BLOCKED_GRACE`]; by how
/// much it is.
async fn blocked(clock: &OpClock) -> Duration {
    loop {
        tokio::time::sleep(WATCH_TICK).await;
        if let Some(deadline) = clock.deadline() {
            let over = std::time::Instant::now().saturating_duration_since(deadline);
            if over > BLOCKED_GRACE {
                return over;
            }
        }
    }
}

/// A part of the bridge that ended on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Died {
    pub part: &'static str,
    pub cause: String,
}

impl Running {
    /// The first part of the bridge to end — none ends on its own, so it is a death: the MQTT side
    /// (`mqtt`) or the device worker (`device`), also a device worker blocked past its operation's
    /// deadline (then the device is said unreachable and the momentary commands waiting are
    /// refused first). Cancellation-safe.
    pub async fn ended(&mut self) -> Died {
        let watching = self.watching && self.worker.is_some();
        // WAIT: bridge-parts
        tokio::select! {
            over = blocked(&self.clock), if watching => {
                self.watching = false;
                self.reports.status(LinkStatus::Unreachable);
                worker::report_refused(&self.reports, self.mailbox.disconnected());
                Died {
                    part: "device",
                    cause: format!("the device worker blocked its thread {} ms past its operation's deadline", over.as_millis()),
                }
            }
            Some(joined) = self.tasks.join_next() => Died { part: "mqtt", cause: match joined {
                Ok(()) => "returned".to_owned(),
                Err(e) => e.to_string(),
            }},
            end = async {
                match self.worker.as_mut() {
                    // WAIT: bridge-parts
                    Some(worker) => worker.await,
                    // WAIT: bridge-parts
                    None => std::future::pending().await,
                }
            } => {
                self.worker = None;
                Died { part: "device", cause: match end {
                    Ok(WorkerEnd::Died(cause)) => cause,
                    Ok(WorkerEnd::Stopped) => "stopped".to_owned(),
                    Err(_) => "its thread ended without a word".to_owned(),
                }}
            }
        }
    }

    /// The bounded shutdown: `Active=false` and DISCONNECT, the device worker stopped and its
    /// driver disconnected, within [`SHUTDOWN_GRACE`]. Past it, one WARN `shutdown_timeout`, and
    /// what is left is abandoned. Returns whether it ended in time.
    pub async fn stop(mut self) -> bool {
        let span = self.span.clone();
        async move {
            let _ = self.shutdown.send(true);
            let _ = self.worker_stop.send(true);
            let worker = self.worker.take();
            let tasks = &mut self.tasks;
            let all = async move {
                // WAIT: shutdown
                while tasks.join_next().await.is_some() {}
                if let Some(worker) = worker {
                    // WAIT: shutdown
                    let _ = worker.await;
                }
            };
            let ended = stop_within(SHUTDOWN_GRACE, all).await;
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
pub(crate) async fn stop_within<T>(grace: Duration, stop: impl Future<Output = T>) -> bool {
    // WAIT: shutdown
    let ended = tokio::time::timeout(grace, stop).await.is_ok();
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
    running.stop().await;
    ending
}

/// The test seam's environment variable (debug builds only): set, the stop signals cannot be
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
        #[cfg(debug_assertions)]
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

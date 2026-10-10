//! The multi-device host (multi-device spec, tracing-init
//! `docs/superpowers/specs/2026-10-10-multi-device-host-design.md`, §3, §4): one process, one MQTT
//! session, many devices — a device-direct bridge (mqtt_ynca's receivers, the IR emitters) — each
//! device built from its instance's retained `Config/{instance}` by the bridge's factory.
//!
//! ```no_run
//! # use mqtt_bridge_kit::*;
//! # struct Receiver;
//! # impl DeviceDriver for Receiver {
//! #     fn info(&self) -> DriverInfo { DriverInfo::default() }
//! #     async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> { Ok(()) }
//! #     async fn disconnect(&mut self) {}
//! #     async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> { Ok(vec![]) }
//! #     async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> { Ok(()) }
//! #     async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> { Ok(None) }
//! #     async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> { Ok(()) }
//! # }
//! fn main() -> std::process::ExitCode {
//!     Host::new("ynca")
//!         .root("Ynca")
//!         .version("2.0.0")
//!         .from_cli()
//!         .device(|config: &DeviceConfig| match config.fields().get("address").and_then(Value::as_str) {
//!             Some(_address) => Ok(Receiver),
//!             None => Err(FactoryError::Malformed("no `address`".into())),
//!         })
//!         .run()
//! }
//! ```
//!
//! **What an instance is.** The Store publishes, retained, `{Root}/Config/{instance}` =
//! `{"bridge": "<kind>", "via": "<bridge id>", …driver fields…, "disabled"?: bool}`. A process owns
//! the instance iff its last accepted Config names it (`via`, or — omitted — the default bridge
//! id of the kind the Config names in `bridge`, that kind itself) and the kind matches (§3, §4.2). It then subscribes the instance's
//! own lanes in three steps (S1 its read-back: `State/{i}/+` and `Status/{i}`; S2 its target
//! Configs; S3 its requests), each SUBACK matched by packet id, builds the driver off to the side
//! within `--factory-timeout`, and starts the device — R1's worker, mailbox and reports, its own
//! `Status` — once both are done (§4.3).
//!
//! **Failure domain (D4 = a).** A device whose worker thread is blocked, whose blocking budget
//! stays exhausted, whose driver panics (or whose worker ends unasked), or whose factory or `info()`
//! does not return in time ends the process (R1's dead-worker path: the bounded shutdown, a failure
//! exit, the supervisor's restart). Every other
//! device failure is that device's lost link, handled per device.
//!
//! **Retained topics.** Every `Status` and `State` of a configured instance carries exactly one
//! MQTT v5 user property `via` = this process's bridge id; the owner republishes its value over a
//! foreign write or a retraction of its slot (healing), and on a delete retracts the topics whose
//! current value carries its own tag (§4.5).
//!
//! **One runtime.** A [`Bridge`](crate::Bridge) — a controller-fronted bridge, one device per
//! process — is this host's N = 1 mode: one fixed instance, named from `--instance`, built from the
//! driver `main` hands it rather than from a Config. Its owner is the process itself (§4.2), so it
//! has no `Config/+` ownership and no barrier, carries no `via` tag, and says `Active/{instance}`;
//! its CONNACK duties, routing and wire are R1's, through the same session, lifecycle, publication
//! rotation, packet-id budget and shutdown flush as a configured instance's.
//!
//! The session side of all this is in [`crate::session`], and the per-instance lifecycle in the
//! crate's private `lifecycle` module; this module is the public builder and the factory's types.

use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;

use crate::config::{self, Problem, Settings, TimingOverrides};
use crate::devices::{Spawner, Startable};
use crate::driver::{DeviceDriver, DriverInfo, PropertyMap, Timing};
use crate::lifecycle::{self, Lifecycle};
use crate::process::Running;
use crate::pump::BrokerOutage;
use crate::session::{self, MqttTiming, Outputs};
use crate::topics::Topics;

/// An instance's Config as the factory gets it: the instance's name and its driver fields — every
/// field of `Config/{instance}` but the host's own (`bridge`, `via`, `disabled`).
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceConfig {
    instance: String,
    fields: PropertyMap,
}

impl DeviceConfig {
    /// A Config for `instance` with these driver fields (for a factory's tests).
    pub fn new(instance: impl Into<String>, fields: PropertyMap) -> DeviceConfig {
        DeviceConfig { instance: instance.into(), fields }
    }

    /// The instance: one topic segment, unique on the root.
    pub fn instance(&self) -> &str {
        &self.instance
    }

    /// The driver fields (an address, a port's code set…): everything but `bridge`, `via` and
    /// `disabled`.
    pub fn fields(&self) -> &PropertyMap {
        &self.fields
    }
}

/// Why a factory could not build a driver from a Config (multi-device spec §3, "Config errors").
/// Either way the instance gets one `Error/{instance}` naming the reason, the instance's last
/// accepted Config stays in force (none: the instance is *failed*, `Status=unreachable`, retried at
/// the reconnect pacing and on any Config change), and WARN `device_config_rejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactoryError {
    /// The driver fields are invalid (a missing or ill-typed field): `Error` reason `malformed`.
    Malformed(String),
    /// The fields are valid but no driver can be built from them now: `Error` reason `unusable`.
    Unusable(String),
}

impl std::fmt::Display for FactoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FactoryError::Malformed(m) => write!(f, "malformed: {m}"),
            FactoryError::Unusable(m) => write!(f, "unusable: {m}"),
        }
    }
}

impl std::error::Error for FactoryError {}

/// The factory, its driver type erased.
pub(crate) type Factory = Arc<dyn Fn(&DeviceConfig) -> Result<Box<dyn crate::devices::Startable>, FactoryError> + Send + Sync>;

/// A multi-device bridge process, as `main` builds it: its kind, its command line and its drivers'
/// options, then the factory ([`device`](Host::device)) and [`run`](Host::run). Every setter takes
/// and returns `self`. (A controller-fronted bridge — one device per process — is a
/// [`Bridge`](crate::Bridge): this host with one fixed instance, N = 1.)
///
/// The command line is a [`Bridge`](crate::Bridge)'s, with `--name <id>` (this process's bridge id,
/// unique on the root; by default the kind) and `--factory-timeout <d>` (the bound on a device's
/// construction; 10 s by default) in place of `--instance`. The driver timing flags apply to every
/// device, over each driver's own `Timing`.
#[derive(Clone)]
pub struct Host {
    kind: String,
    program: String,
    app_name: Option<String>,
    version: Option<String>,
    declared: Vec<(String, String)>,
    required: Vec<String>,
    parsed: bool,
    settings: Settings,
    problem: Option<Problem>,
    factory: Option<Factory>,
    /// N = 1: a [`Bridge`](crate::Bridge)'s one fixed instance (`--instance`), its driver handed
    /// over at the start; no factory, no Configs.
    fixed: bool,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host")
            .field("kind", &self.kind)
            .field("fixed", &self.fixed)
            .field("settings", &self.settings)
            .field("problem", &self.problem)
            .finish()
    }
}

/// The program's name: its executable's file name, else `fallback`.
fn program(fallback: String) -> String {
    std::env::args_os()
        .next()
        .and_then(|a| std::path::Path::new(&a).file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or(fallback)
}

impl Host {
    /// A host of `kind` (`ynca`, `globalcache`): the implementation kind a Config's `bridge` field
    /// names, the default bridge id, the first word of the retained `Version`, and the default root
    /// ([`root`](Self::root) sets the family's root in code, `--root` overrides it). Nothing is read
    /// from the command line until [`from_cli`](Self::from_cli) or [`args`](Self::args).
    pub fn new(kind: &str) -> Host {
        Host {
            kind: kind.to_owned(),
            program: program(kind.to_owned()),
            app_name: None,
            version: None,
            declared: Vec::new(),
            required: Vec::new(),
            parsed: false,
            settings: Settings::new(kind),
            problem: None,
            factory: None,
            fixed: false,
        }
    }

    /// A [`Bridge`](crate::Bridge)'s host: one fixed instance of `family` (the default root), its
    /// command line R1's (`--instance`, no `--name`), its `Version` text R1's.
    pub(crate) fn fixed(family: &str) -> Host {
        Host { program: program(family.to_lowercase()), fixed: true, ..Host::new(family) }
    }

    /// The kind.
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// Declare an option the drivers take, as [`Bridge::option`](crate::Bridge::option) does.
    pub fn option(mut self, name: &str, help: &str) -> Host {
        self.declared.push((name.to_owned(), help.to_owned()));
        self
    }

    /// Make a declared option required, as [`Bridge::require`](crate::Bridge::require) does.
    pub fn require(mut self, name: &str) -> Host {
        self.required.push(name.to_owned());
        if self.parsed {
            self.check_required();
        }
        self
    }

    /// A usage error found in the options, as [`Bridge::usage_error`](crate::Bridge::usage_error).
    pub fn usage_error(mut self, message: impl Into<String>) -> Host {
        if self.problem.is_none() {
            self.problem = Some(Problem::Usage(message.into()));
        }
        self
    }

    /// The command line's own end — `--help` (exit 0) or a usage error (exit 2) said — before
    /// anything starts; `None` when the host is to run. As
    /// [`Bridge::usage_exit`](crate::Bridge::usage_exit).
    pub fn usage_exit(&self) -> Option<ExitCode> {
        let usage = || match self.fixed {
            true => config::usage(&self.program, &self.kind, &self.declared),
            false => config::usage_host(&self.program, &self.kind, &self.settings.root, &self.declared),
        };
        match &self.problem {
            Some(Problem::Help) => {
                // The command line's own output, before anything has started: exempt.
                print!("{}", usage());
                Some(ExitCode::SUCCESS)
            }
            Some(Problem::Usage(message)) => {
                eprint!("{}: {message}\n\n{}", self.program, usage());
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

    /// The logging's application name; by default the program's name.
    pub fn app_name(mut self, name: &str) -> Host {
        self.app_name = Some(name.to_owned());
        self
    }

    /// The bridge's version: the retained `Version/{bridge id}` says `<kind> <version>`
    /// (`globalcache 1.2.0`), which the Store's Admin UI reads to offer this process for an
    /// instance's `Via` (multi-device spec §3, §5.2).
    pub fn version(mut self, version: &str) -> Host {
        self.version = Some(version.to_owned());
        self
    }

    /// Read the process's command line.
    pub fn from_cli(self) -> Host {
        self.args(std::env::args().skip(1))
    }

    /// Read `args` as the command line (without the program's name).
    pub fn args(mut self, args: impl IntoIterator<Item = String>) -> Host {
        self.parsed = true;
        let parse = if self.fixed { config::parse } else { config::parse_host };
        if let Err(problem) = parse(args, &mut self.settings, &self.declared) {
            self.problem = Some(problem);
        }
        self.check_required();
        self
    }

    /// A declared option's value, if the command line gave one.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.settings.options.get(name).map(String::as_str)
    }

    /// The drivers' options the command line gave.
    pub fn values(&self) -> &BTreeMap<String, String> {
        &self.settings.options
    }

    /// Set the root (in code — the family's, e.g. `IRemitter`; `--root` on the command line).
    pub fn root(mut self, root: &str) -> Host {
        self.settings.root = root.to_owned();
        self
    }

    /// Set the fixed instance (a [`Bridge`](crate::Bridge)'s; `--instance` on its command line).
    pub(crate) fn instance(mut self, instance: &str) -> Host {
        self.settings.instance = Some(instance.to_owned());
        self
    }

    /// Set this process's bridge id (in code; `--name` on the command line): unique on the root, by
    /// default the kind. Several processes of one kind share a root under distinct ids.
    pub fn name(mut self, id: &str) -> Host {
        self.settings.name = Some(id.to_owned());
        self
    }

    /// Set the broker, `host[:port]` (in code; `--broker` on the command line).
    pub fn broker(mut self, broker: &str) -> Host {
        self.settings.broker = Some(broker.to_owned());
        self
    }

    /// Override every driver's timing (in code; one flag each on the command line).
    pub fn timing(mut self, overrides: TimingOverrides) -> Host {
        self.settings.timing = overrides;
        self
    }

    /// A device link that lasts this long ends its outage (in code; `--device-stable`).
    pub fn device_stable(mut self, stable: Duration) -> Host {
        self.settings.device_stable = Some(stable);
        self
    }

    /// Set the MQTT side's timing (in code; `--mqtt-…` on the command line).
    pub fn mqtt_timing(mut self, timing: MqttTiming) -> Host {
        self.settings.mqtt = timing;
        self
    }

    /// The bound on a device's construction (in code; `--factory-timeout`): the factory, and then
    /// the driver's `info()`, each within it. One that does not return ends the process (D4 = a).
    /// 10 s by default.
    pub fn factory_timeout(mut self, bound: Duration) -> Host {
        self.settings.factory_timeout = Some(bound);
        self
    }

    /// The settings as given.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The factory: a driver from an instance's Config — the driver fields checked, the device's
    /// address taken — or why not ([`FactoryError`]). It runs on a thread of its own under
    /// `--factory-timeout`, and must not wait for the device: connecting is the driver's `connect`,
    /// which the runtime calls, paced and bounded, once the device starts.
    pub fn device<D, F>(mut self, factory: F) -> Host
    where
        D: DeviceDriver,
        F: Fn(&DeviceConfig) -> Result<D, FactoryError> + Send + Sync + 'static,
    {
        self.factory = Some(Arc::new(move |config: &DeviceConfig| {
            factory(config).map(|driver| Box::new(crate::devices::Built(driver)) as Box<dyn crate::devices::Startable>)
        }));
        self
    }

    /// The logging's application name.
    pub(crate) fn app(&self) -> String {
        self.app_name.clone().unwrap_or_else(|| self.program.clone())
    }

    /// `main`'s whole body: says `--help` or a usage error and returns its code before anything
    /// else; otherwise installs the panic hook, builds the tokio runtime, takes the stop signals,
    /// starts the logging, starts the host ([`start`](Self::start)) and blocks until SIGINT or
    /// SIGTERM (the bounded shutdown, `ExitCode::SUCCESS`) or a dead part — a device's blocked
    /// worker, exhausted budget or overdue factory among them (ERROR `worker_died`, the bounded
    /// shutdown, `ExitCode::FAILURE`, for the service manager to restart). As
    /// [`Bridge::run`](crate::Bridge::run).
    pub fn run(self) -> ExitCode {
        if let Some(code) = self.usage_exit() {
            return code;
        }
        let app = self.app();
        let log_config = self.settings.log_config.clone();
        crate::process::run_process(app, log_config, move || self.start())
    }

    /// Start the host on the current tokio runtime, in-process (no logging start, no signals, no
    /// panic hook — [`run`](Self::run) adds those), as [`Bridge::start`](crate::Bridge::start). A
    /// part's death reaches the caller only through [`Running::ended`].
    pub fn start(self) -> Result<Running, String> {
        self.launch(None)
    }

    /// Start either mode on the current runtime: a configured host, whose factory builds each
    /// instance's driver; or a [`Bridge`](crate::Bridge)'s fixed instance (N = 1), with `fixed`'s
    /// driver — and its `info()`, if [`Bridge::run`](crate::Bridge::run) took it off the runtime
    /// already, else taken here. One session, one lifecycle, either way.
    pub(crate) fn launch(self, fixed: Option<(Box<dyn Startable>, Option<DriverInfo>)>) -> Result<Running, String> {
        if let Some(problem) = &self.problem {
            return Err(match problem {
                Problem::Help => "--help".to_owned(),
                Problem::Usage(m) => m.clone(),
            });
        }
        // The bridge id: a fixed host's is its instance — `Active/{instance}`, the client id — and
        // a configured host's stands where it would.
        let id = match self.fixed {
            true => self.settings.instance.clone().ok_or("no instance")?,
            false => self.settings.name.clone().unwrap_or_else(|| self.kind.clone()),
        };
        let topics = Topics::new(&self.settings.root, &id)?;
        let (broker_host, broker_port) = config::broker_host_port(self.settings.broker.as_deref().ok_or("no broker")?)?;
        let (span, version, factory, fixed) = match (self.fixed, fixed) {
            (true, Some((driver, info))) => {
                let span = tracing::info_span!("bridge", root = %topics.root(), instance = %id);
                let info = info.unwrap_or_else(|| driver.info());
                // The driver's timing, with the command line's overrides: timings that cannot run
                // are the start's failure.
                let info = config::device_info(info, &self.settings.timing, self.settings.device_stable, &self.settings.mqtt)?;
                let version =
                    self.version.clone().unwrap_or_else(|| format!("{} (mqtt-bridge-kit {})", self.app(), env!("CARGO_PKG_VERSION")));
                (span, version, None, Some((driver, info)))
            }
            (false, None) => {
                let factory = self.factory.clone().ok_or("no factory: `device` names one before the host starts")?;
                // Each device's timing is checked once its driver says it; the MQTT side's, and the
                // overrides on a default timing, now.
                config::check_timing(&self.settings.timing.apply(Timing::default()), &self.settings.mqtt)?;
                let span = tracing::info_span!("host", root = %topics.root(), bridge = %id);
                let version = format!(
                    "{} {}",
                    self.kind,
                    self.version.clone().unwrap_or_else(|| format!("(mqtt-bridge-kit {})", env!("CARGO_PKG_VERSION")))
                );
                (span, version, Some(factory), None)
            }
            (true, None) => return Err("a fixed bridge starts with its driver".to_owned()),
            (false, Some(_)) => return Err("a host builds its drivers with its factory".to_owned()),
        };
        let ready = Arc::new(Notify::new());
        let shared = Arc::new(session::Shared {
            topics,
            broker_host,
            broker_port,
            timing: self.settings.mqtt,
            version,
            outage: Arc::new(BrokerOutage::new(self.settings.mqtt.warn_after, self.settings.mqtt.stable_after)),
            enqueued: std::sync::atomic::AtomicU64::new(0),
            ready: ready.clone(),
            devices: lock_order::sync::Mutex::new("session-devices", Vec::new()),
        });
        let (spawner, deaths, events) = Spawner::new();
        let registry = spawner.registry.clone();
        let settings = lifecycle::Settings {
            kind: self.kind.clone(),
            id: id.clone(),
            root: self.settings.root.clone(),
            factory,
            factory_timeout: self.settings.factory_timeout.unwrap_or(config::FACTORY_TIMEOUT),
            // A failed construction is retried at the reconnect pacing: `--retry`, or the default.
            retry: self.settings.timing.retry.unwrap_or(Timing::default().retry),
            overrides: self.settings.timing,
            device_stable: self.settings.device_stable,
            mqtt: self.settings.mqtt,
        };
        let mut lifecycle = Lifecycle::new(settings, shared.clone(), spawner, span.clone(), ready);
        let mut outputs = Outputs::new();
        if let Some((driver, info)) = fixed {
            lifecycle.start_fixed(&mut outputs, &id, driver, info, span.clone())?;
        }
        let (shutdown, shutdown_rx) = watch::channel(false);
        let mut tasks = JoinSet::new();
        let _entered = span.enter();
        session::spawn(shared, lifecycle, outputs, events, shutdown_rx, &mut tasks);
        drop(_entered);
        Ok(Running::new(tasks, shutdown, span, registry, deaths))
    }
}

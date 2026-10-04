//! The driver API (Bridge Runtime spec §4, §6): what a bridge author implements, and what the
//! runtime owns around it.
//!
//! A driver is one object, called by one runtime task at a time — so a device protocol that needs
//! strict sequencing gets it for free. Every call carries a deadline the runtime enforces: an
//! overrun counts as a lost link, never a hang. The driver runs on a thread of its own with a
//! single-threaded runtime (see [`crate::worker`]), so its futures need not be `Send`; the driver
//! itself moves to that thread, hence `Send + 'static`.

use std::fmt;
use std::time::Duration;

pub use serde_json::Value;

use crate::link::LinkContext;

/// Property values by name: a target's state, or the values of one atomic write.
pub type PropertyMap = serde_json::Map<String, Value>;

/// What the runtime asks the driver to read (`read_state`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every target the device has.
    All,
    /// One target.
    Target(String),
    /// Whatever is cheapest to read: the default `probe` reads this to prove the device alive.
    Any,
}

/// One target's observed state, as `read_state` returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct TargetState {
    pub target: String,
    pub values: PropertyMap,
}

impl TargetState {
    pub fn new(target: impl Into<String>, values: PropertyMap) -> TargetState {
        TargetState { target: target.into(), values }
    }
}

/// A momentary command, as `execute` gets it: its name and its parameters (every field of the
/// `Command` payload other than `target` and `command`).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandCall {
    pub command: String,
    pub params: PropertyMap,
}

/// The connection failed: I/O, a timeout, the device closing it (spec §4.3, `Link`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError {
    reason: String,
}

impl LinkError {
    pub fn new(reason: impl Into<String>) -> LinkError {
        LinkError { reason: reason.into() }
    }

    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for LinkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.reason)
    }
}

impl std::error::Error for LinkError {}

impl From<std::io::Error> for LinkError {
    fn from(e: std::io::Error) -> LinkError {
        LinkError::new(e.to_string())
    }
}

/// How an operation failed (spec §4.3). The runtime's answer to each:
/// - `Link`: drops the link, starts or continues the outage episode, holds state requests,
///   refuses momentary ones, and reconnects at the paced rate;
/// - `Rejected`: publishes `Error` naming the target and property (the Store abandons the
///   request), logs WARN `command_rejected`; the link stays up;
/// - `Unusable`: a reply the driver could not parse — publishes `Error` once and passes the request
///   over; never reconnects for it (no-hang 3b: reconnecting for it was a hot loop);
/// - `Unsupported`: no such command or property — publishes `Error`; no retry;
/// - `Partial` (`apply` only): the properties it names failed, each with its class, and every
///   other value of the write was applied (unproven, for unacknowledged writes) — `Error` names
///   only the failed ones. A plain `Rejected`, `Unusable` or `Unsupported` means the whole write
///   failed. From any other operation it counts as `Rejected`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpError {
    Link(LinkError),
    Rejected(String),
    Unusable(String),
    Unsupported(String),
    Partial(Vec<PropertyFailure>),
}

/// One property an `apply` could not write (`OpError::Partial`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyFailure {
    pub property: String,
    pub class: FailureClass,
    pub error: String,
}

/// How one property of a partial `apply` failed: the classes of `OpError` a request can fail by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    Rejected,
    Unusable,
    Unsupported,
}

impl From<LinkError> for OpError {
    fn from(e: LinkError) -> OpError {
        OpError::Link(e)
    }
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpError::Link(e) => write!(f, "link: {e}"),
            OpError::Rejected(m) => write!(f, "rejected: {m}"),
            OpError::Unusable(m) => write!(f, "unusable: {m}"),
            OpError::Unsupported(m) => write!(f, "unsupported: {m}"),
            OpError::Partial(failures) => {
                write!(f, "partial:")?;
                for failure in failures {
                    write!(f, " {} ({:?}: {})", failure.property, failure.class, failure.error)?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for OpError {}

/// What a successful `apply` means (spec §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writes {
    /// The device confirmed it (CoolMaster).
    Acknowledged,
    /// It was only sent (the HWI, YNCA): each applied value stays unproven until a later
    /// successful `probe` or read, and is held again if the link dies first, unless a newer value
    /// waits.
    Unacknowledged,
}

/// Where a target's state comes from (spec §4.4). The driver's default; a target's `Config` may
/// override it with `{"feedback": "assumed"}` or `{"feedback": "observed"}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feedback {
    /// Read back from the device.
    Observed,
    /// A one-way device: the runtime publishes applied values as `State` marked
    /// `"assumed": true`, unless the driver reports otherwise.
    Assumed,
}

impl Feedback {
    pub fn parse(text: &str) -> Option<Feedback> {
        match text {
            "observed" => Some(Feedback::Observed),
            "assumed" => Some(Feedback::Assumed),
            _ => None,
        }
    }
}

/// A command's class (spec §4.4). `refresh` and `assume` are standard and handled by the runtime;
/// a driver does not list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandClass {
    /// Act once; queued in order while the device is up (at most 64), refused while it is down.
    Momentary,
    /// A read: coalesced per target and command; dropped while the device is down.
    Read,
    /// Like `Momentary`, with an answer for the command's response topic.
    WithReply,
}

/// A command the driver executes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: String,
    pub class: CommandClass,
}

impl CommandSpec {
    pub fn new(name: impl Into<String>, class: CommandClass) -> CommandSpec {
        CommandSpec { name: name.into(), class }
    }
}

/// The driver's timing defaults (spec §4.4); each can be overridden on the command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    /// The bound on `connect`.
    pub connect: Duration,
    /// The bound on every other operation (`read_state`, `apply`, `execute`, `configure`).
    pub operation: Duration,
    /// Connects are at most one per this (after a failed connect and after a lost link alike).
    pub retry: Duration,
    /// A device outage this long is one WARN (`device_unreachable`).
    pub outage_warn: Duration,
    /// `read_state(Scope::All)` this often while the device is up; `None` never.
    pub poll: Option<Duration>,
    /// For `Writes::Unacknowledged`: a device silent this long is probed.
    pub probe_interval: Duration,
    /// The bound on `probe`.
    pub probe_deadline: Duration,
    /// A device outage ends once a recovered link — proven, its catch-up done — has lasted this
    /// long; one lost sooner is one more attempt of the same outage (review MINOR 10; the broker
    /// side's `MqttTiming::stable_after`). `--device-stable` overrides it.
    pub stable: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            connect: Duration::from_secs(10),
            operation: Duration::from_secs(10),
            retry: Duration::from_secs(5),
            outage_warn: Duration::from_secs(30),
            poll: None,
            probe_interval: Duration::from_secs(30),
            probe_deadline: Duration::from_secs(10),
            stable: Duration::from_secs(5),
        }
    }
}

/// What a driver declares about itself (spec §4.4), once, at start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverInfo {
    pub writes: Writes,
    pub feedback: Feedback,
    pub commands: Vec<CommandSpec>,
    pub timing: Timing,
    /// Restore a one-way device after the bridge restarts: at the process's first connect, each
    /// target with assumed feedback gets the read-back's values — what the previous run said it had
    /// set — before any held value. For a device that comes back dark (DMX). Off by default; the
    /// command line's `--restore on|off` overrides it.
    pub restore: bool,
}

impl Default for DriverInfo {
    fn default() -> DriverInfo {
        DriverInfo {
            writes: Writes::Acknowledged,
            feedback: Feedback::Observed,
            commands: Vec::new(),
            timing: Timing::default(),
            restore: false,
        }
    }
}

/// A device driver (spec §6). See the module documentation for how it is called.
///
/// | Operation | Called when | Must |
/// |---|---|---|
/// | `info` | once, at start | be pure |
/// | `connect` | to open the device link (paced retries) | finish or fail within the connect timing; may start helpers through `ctx` |
/// | `disconnect` | after a lost link, at shutdown | be quick; best effort |
/// | `read_state` | after connecting, on the poll interval, for `refresh`, before resolving outage conflicts | read the device, not a cache |
/// | `apply` | for a request; several values together for an atomic write | apply every value, in the order the device needs |
/// | `execute` | for a momentary command | act once; never retry |
/// | `configure` | when a `Config` intent arrives or is retracted (`None`) | accept the setup |
/// | `probe` | for `Writes::Unacknowledged`: idle and while busy | prove the device alive; defaults to a read |
#[allow(async_fn_in_trait)] // the runtime is generic over the driver and polls its futures on one thread: no `Send` bound is needed
pub trait DeviceDriver: Send + 'static {
    fn info(&self) -> DriverInfo;
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError>;
    async fn disconnect(&mut self);
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError>;
    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError>;
    /// `target`: the command's, or `None` for one that names none.
    async fn execute(&mut self, target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError>;
    async fn configure(&mut self, target: &str, config: Option<&Value>) -> Result<(), OpError>;
    async fn probe(&mut self) -> Result<(), OpError> {
        self.read_state(Scope::Any).await.map(drop)
    }
}

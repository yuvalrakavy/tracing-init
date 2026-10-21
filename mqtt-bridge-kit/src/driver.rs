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

use crate::generations::{ContextualState, RequestContext, RequestKind, RequestPolicy};
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
    /// The target's name: one topic segment (no `/`, `+`, `#`, not empty).
    pub target: String,
    /// The properties read, by name. A property the device did not give is left out, not `null`.
    pub values: PropertyMap,
}

impl TargetState {
    /// A target's observed values.
    pub fn new(target: impl Into<String>, values: PropertyMap) -> TargetState {
        TargetState { target: target.into(), values }
    }
}

/// A momentary command, as `execute` gets it: its name and its parameters (every field of the
/// `Command` payload other than `target` and `command`).
#[derive(Debug, Clone, PartialEq)]
pub struct CommandCall {
    /// The command's name, as the `Command` payload gave it (`refresh` and `assume` never reach the
    /// driver: the runtime handles them).
    pub command: String,
    /// The payload's other fields; empty when it had none.
    pub params: PropertyMap,
}

/// The connection failed: I/O, a timeout, the device closing it (spec §4.3, `Link`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkError {
    reason: String,
}

impl LinkError {
    /// A lost link, with the reason to log and to report (it becomes the `Error` text of what the
    /// loss refuses).
    pub fn new(reason: impl Into<String>) -> LinkError {
        LinkError { reason: reason.into() }
    }

    /// The reason, as given.
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
    /// The connection failed (I/O, a timeout, closed): the link is lost. Also what a missed
    /// deadline becomes, without the driver returning it. `From<LinkError>` makes `?` work.
    Link(LinkError),
    /// The device refused this request; the link is fine. The text is for the `Error` payload.
    Rejected(String),
    /// The device answered with something the driver could not parse. The text is for the `Error`
    /// payload.
    Unusable(String),
    /// No such command or property. The text is for the `Error` payload.
    Unsupported(String),
    /// `apply` only: these properties failed and every other value was applied.
    Partial(Vec<PropertyFailure>),
}

/// One property an `apply` could not write (`OpError::Partial`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropertyFailure {
    /// The property that was not written.
    pub property: String,
    /// How it failed.
    pub class: FailureClass,
    /// The text for the property's `Error` payload.
    pub error: String,
}

/// How one property of a partial `apply` failed: the classes of `OpError` a request can fail by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// The device refused this value (as [`OpError::Rejected`]); WARN `command_rejected`.
    Rejected,
    /// The device's answer for this property could not be parsed (as [`OpError::Unusable`]).
    Unusable,
    /// The device has no such property (as [`OpError::Unsupported`]).
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
    /// A successful `apply` is final: the device confirmed it (CoolMaster), or nothing more can
    /// ever be learned of it — a one-way transport with nothing to probe (DMX, DALI behind a
    /// write-only interface). Such a driver declares this honestly, and needs no `probe`.
    Acknowledged,
    /// It was only sent, and a later answer can prove it (the HWI, YNCA): each applied value stays
    /// unproven until a later successful `probe` or read, and is held again if the link dies
    /// first, unless a newer value waits. Such a driver must be able to probe: a `probe` answering
    /// `Unsupported` is a contract violation.
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
    /// `"observed"` or `"assumed"`, as a target's `Config` spells its `feedback` override; anything
    /// else is `None`.
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
    /// The command's name, as the `Command` payload spells it.
    pub name: String,
    /// How the runtime queues and answers it.
    pub class: CommandClass,
}

impl CommandSpec {
    /// A command the driver executes.
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

/// Connect and operations 10 s, retry 5 s, outage WARN 30 s, no polling, probe every 30 s with a
/// 10 s deadline, device stable after 5 s.
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
    /// What a successful `apply` means.
    pub writes: Writes,
    /// The default for every target; a target's `Config` may override it.
    pub feedback: Feedback,
    /// The commands `execute` takes, besides the standard `refresh` and `assume`. A `Command`
    /// naming anything else is answered `unsupported` by the runtime, without calling the driver.
    pub commands: Vec<CommandSpec>,
    /// The timing defaults; the command line overrides each.
    pub timing: Timing,
}

/// An acknowledged-writes, observed-feedback driver with no commands and the default timing.
impl Default for DriverInfo {
    fn default() -> DriverInfo {
        DriverInfo { writes: Writes::Acknowledged, feedback: Feedback::Observed, commands: Vec::new(), timing: Timing::default() }
    }
}

/// A device driver (spec §6): the one object a bridge writes. The runtime owns everything around
/// it (see the crate documentation) and calls it from one task at a time, so a protocol that needs
/// strict sequencing gets it for free. The driver runs on a thread of its own with a
/// single-threaded runtime: its futures need not be `Send`, but the driver itself moves there, so
/// it is `Send + 'static`.
///
/// **Deadlines.** Every call except `info` is bounded by the runtime, from [`Timing`] (each
/// overridable on the command line): `connect` by `connect`, `probe` by `probe_deadline`,
/// `disconnect` by `operation` after a lost link and by a fixed two seconds at shutdown, the rest by
/// `operation`. The bound drops the call's future,
/// so a driver's own awaits are cancelled at the deadline, and an overrun is a lost link
/// ([`OpError::Link`]), never a hang. A call that blocks its *thread* cannot be interrupted: past
/// its deadline and a two-second grace the watchdog declares the worker dead and the process exits
/// with a failure for the supervisor to restart. A synchronous library call therefore goes through
/// [`LinkContext::blocking`] / [`LinkHandle::blocking`](crate::link::LinkHandle::blocking), which holds it to the deadline on a thread
/// that can be abandoned.
///
/// **Errors.** An operation answers with an [`OpError`]; its class says what the runtime does (see
/// the type). The one rule to keep: report only a failure of the *connection* as `Link`. A device
/// that answers "no" is `Rejected`; an answer the driver cannot read is `Unusable`, which never
/// reconnects.
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
    /// What the driver declares about itself: writes, feedback, commands, timing. Called once at
    /// start, on a blocking thread raced against the stop signals; it must be pure and quick (no
    /// I/O): one that does not return cannot hold up a stop, but the bridge does not start.
    fn info(&self) -> DriverInfo;

    /// Original inventory mapping versions, captured once alongside [`Self::info`].
    /// Pure and quick, with no I/O. Fencing currently requires a fixed [`crate::Bridge`];
    /// a dynamic Host factory returning Fenced is rejected before its driver starts.
    fn request_policy(&self) -> RequestPolicy {
        RequestPolicy::Unfenced
    }

    /// Additional pure validation before cached success or outage resolution.
    /// The runtime independently checks generation fences. No I/O or waits here;
    /// the ordinary operation clock/watchdog covers this call. An error refuses
    /// the entire request; Partial is treated as Rejected without assuming any
    /// value was applied. Use Rejected/Unsupported for invalid intent, not Link.
    fn validate_request(&self, _target: Option<&str>, _context: &RequestContext, _kind: RequestKind<'_>) -> Result<(), OpError> {
        Ok(())
    }

    /// Open the device link. Called again, at most one per [`Timing::retry`], after every failed
    /// connect and every lost link; bounded by [`Timing::connect`]. `ctx` lives as long as this
    /// link: helpers started through it (an unsolicited-input reader) are cancelled when the link
    /// drops. The context is only handed here; keep a [`LinkHandle`](crate::link::LinkHandle) (`ctx.handle()`) for later
    /// operations. Return a [`LinkError`] if the connection cannot be made.
    ///
    /// Connecting is not yet proof of life: the runtime then reads the device
    /// (`read_state(Scope::All)`, or `probe` if that is unsupported) and only a good answer makes
    /// the link `Up`. Report what the connect learns through `ctx.report` as it goes.
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError>;

    /// Close the device link: called after a lost link (before the next connect) and at shutdown.
    /// Best effort and quick: it is bounded (see the trait's deadlines) and cannot fail. It is
    /// called after every link that ended, a failed `connect` included, so it must cope with a
    /// half-opened link.
    async fn disconnect(&mut self);

    /// Read the device and return the observed state: the bridge publishes it as `State` and
    /// uses it as proof of life. Called after connecting, every [`Timing::poll`], for `refresh`
    /// and before the runtime resolves outage conflicts. Read the device, never a cache.
    ///
    /// A [`Scope::Target`] read returns that target; `All` returns every target the device has
    /// (the runtime never withdraws a target by its absence: a target gone from the device is withdrawn
    /// with [`Reporter::remove`](crate::link::Reporter::remove)). A device that can
    /// only read one address at a time may answer a read of `All` with [`OpError::Unsupported`]:
    /// the runtime then proves the link with `probe` and reads targets one by one when it needs
    /// them. Bounded by [`Timing::operation`].
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError>;

    /// Read observations with their original mapping context. The default keeps
    /// legacy observations unversioned; it never stamps a current generation.
    async fn read_state_with_context(&mut self, scope: Scope) -> Result<Vec<ContextualState>, OpError> {
        self.read_state(scope)
            .await
            .map(|states| states.into_iter().map(|state| ContextualState::new(state, RequestContext::default())).collect())
    }

    /// Read for an original queued refresh or individual conflict check. Drivers
    /// crossing another queue can carry this cause explicitly, including its
    /// captured trace absence. Observation generations still come from actual
    /// device mapping resolution, never by restamping with the request token.
    /// The default preserves existing contextual and plain read implementations.
    /// Unsolicited startup proof and periodic polls use the existing read hook.
    async fn read_state_with_request_context(&mut self, scope: Scope, _context: &RequestContext) -> Result<Vec<ContextualState>, OpError> {
        self.read_state_with_context(scope).await
    }

    /// Write `values` to `target`: several values arrive together when the write is atomic. Apply
    /// every value, in the order the device needs. Bounded by [`Timing::operation`].
    ///
    /// Return `Ok(())` when every value was applied. Otherwise return the class for the whole
    /// write (`Rejected`, `Unusable`, `Unsupported`, or `Link`), or [`OpError::Partial`] naming
    /// exactly the properties that failed: every value not named was applied. The runtime
    /// publishes an `Error` for each failed property alone, and what stood applied stays.
    ///
    /// With [`Writes::Acknowledged`] `Ok` is final. With [`Writes::Unacknowledged`] it only means
    /// "sent": the value stays unproven until a later `probe` or read succeeds, and is applied
    /// again if the link dies first.
    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError>;

    /// Apply values sharing one mapping generation. A merged batch uses its
    /// latest surviving request as the diagnostic cause; property failures keep
    /// each property's original cause in the runtime.
    async fn apply_with_context(&mut self, target: &str, values: &PropertyMap, _context: &RequestContext) -> Result<(), OpError> {
        // WAIT: device-op
        self.apply(target, values).await
    }

    /// Run a momentary command, once: never retry (the runtime does not, and a retry here could
    /// act twice). `target` is the command's target, or `None` for a command that names none.
    /// Bounded by [`Timing::operation`].
    ///
    /// The value returned is the reply of a [`CommandClass::WithReply`] command, sent to the
    /// command's response topic; `Ok(None)` is a plain success. A command of class
    /// [`CommandClass::Read`] is coalesced and dropped while the link is down. Any failure class
    /// is an `Error` with the command's name; `Link` also drops the link.
    async fn execute(&mut self, target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError>;

    /// Execute a command with its original packet context.
    async fn execute_with_context(
        &mut self,
        target: Option<&str>,
        call: &CommandCall,
        _context: &RequestContext,
    ) -> Result<Option<Value>, OpError> {
        // WAIT: device-op
        self.execute(target, call).await
    }

    /// Accept (`Some`) or withdraw (`None`) a target's setup, as its `Config` intent arrived or was
    /// retracted. Called only while the link is up; a config identical to the one last taken on this
    /// link is not handed over again, and a lost link forgets what was taken. A config's `feedback`
    /// override takes effect once this has returned `Ok`. Bounded by [`Timing::operation`].
    async fn configure(&mut self, target: &str, config: Option<&Value>) -> Result<(), OpError>;

    /// Configure a target with the original packet or installed context.
    async fn configure_with_context(&mut self, target: &str, config: Option<&Value>, _context: &RequestContext) -> Result<(), OpError> {
        self.configure(target, config).await
    }

    /// Prove the device is alive, for a driver with [`Writes::Unacknowledged`]: called when the
    /// device has been quiet for [`Timing::probe_interval`], and soon after writes while they are
    /// unproven. The answer must have been caused by something sent after every unproven write: a
    /// successful probe proves them all taken. Bounded by [`Timing::probe_deadline`].
    ///
    /// The default reads ([`Scope::Any`]) and drops the answer. `Link` loses the link; any other
    /// answer, an unusable one included, proves the device alive. [`OpError::Unsupported`] proves
    /// nothing: from an unacknowledged driver it is a contract violation (ERROR
    /// `driver_contract_violation`, once), after which its writes are taken as proven anyway.
    async fn probe(&mut self) -> Result<(), OpError> {
        self.read_state_with_context(Scope::Any).await.map(drop)
    }
}

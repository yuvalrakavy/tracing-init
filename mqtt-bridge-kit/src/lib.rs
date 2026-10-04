//! The Bridge Runtime (tracing-init `docs/superpowers/specs/2026-10-04-bridge-runtime-design.md`):
//! one Rust runtime that owns every MQTT device bridge's policy, so each bridge keeps only its
//! device protocol, as a [`DeviceDriver`].
//!
//! ```no_run
//! # use mqtt_bridge_kit::*;
//! # struct Coolmaster;
//! # impl DeviceDriver for Coolmaster {
//! #     fn info(&self) -> DriverInfo { DriverInfo::default() }
//! #     async fn connect(&mut self, _: &mut LinkContext) -> Result<(), LinkError> { Ok(()) }
//! #     async fn disconnect(&mut self) {}
//! #     async fn read_state(&mut self, _: Scope) -> Result<Vec<TargetState>, OpError> { Ok(vec![]) }
//! #     async fn apply(&mut self, _: &str, _: &PropertyMap) -> Result<(), OpError> { Ok(()) }
//! #     async fn execute(&mut self, _: Option<&str>, _: &CommandCall) -> Result<Option<Value>, OpError> { Ok(None) }
//! #     async fn configure(&mut self, _: &str, _: Option<&Value>) -> Result<(), OpError> { Ok(()) }
//! # }
//! fn main() -> std::process::ExitCode {
//!     mqtt_bridge_kit::Bridge::new("Aircondition").from_cli().run(Coolmaster)
//! }
//! ```
//!
//! What the runtime owns (spec §5), and where:
//!
//! | Area | Module |
//! |---|---|
//! | the topic grammar from `--root` and `--instance`; the subscriptions | [`topics`], [`session`] |
//! | the pump: polls only; an unbounded forward with a backlog episode; rumqttc 0.25's collision fix | [`pump`] |
//! | every CONNACK: subscribe, `Active=true`, `Version`, the retained model again | [`session`] |
//! | the start-up read-back of the bridge's own retained `State` | [`session`], [`reports`] |
//! | requests: both `Desired` forms, the deadline from the message expiry, `conflict`, retractions, malformed payloads answered on `Error` | [`session`], [`wire`] |
//! | the mailbox: latest per (target, property), momentary FIFO (64) refused while down, reads coalesced, held values dropped at their deadline, unproven writes held again | [`mailbox`] |
//! | the worker: paced connects, proof of life, catch-up with the outage conflict rule, polls, probes | [`worker`] |
//! | reports that never wait; capped `Event` and `Error` queues | [`reports`], [`link`] |
//! | replies on the response topic, with the correlation data | [`replies`] |
//! | outage episodes with the fleet's kind names | [`pump`], [`worker`] |
//! | the process: signals first, the logging start raced against the stop, a bounded end | [`process`], [`logging`] |

pub mod config;
pub mod driver;
pub mod link;
pub mod logging;
pub mod mailbox;
pub mod process;
pub mod pump;
pub mod replies;
pub mod reports;
pub mod session;
pub mod topics;
pub mod wire;
pub mod worker;

#[cfg(test)]
mod test_support;

pub use config::TimingOverrides;
pub use driver::{
    CommandCall, CommandClass, CommandSpec, DeviceDriver, DriverInfo, FailureClass, Feedback, LinkError, OpError, PropertyFailure,
    PropertyMap, Scope, TargetState, Timing, Value, Writes,
};
pub use link::{LinkContext, LinkHandle, Reporter};
pub use process::{install_panic_hook, Bridge, Died, Running};
pub use session::MqttTiming;
pub use wire::Conflict;

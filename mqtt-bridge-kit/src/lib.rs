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
//! A driver is one object the runtime calls one operation at a time, under deadlines it enforces;
//! each operation's contract and error classes are on [`DeviceDriver`] and [`OpError`]. A bridge
//! takes one of two shapes (owner decision D1). A controller-fronted bridge is a [`Bridge`]: one
//! process per controller, the controller the topic `instance`, the devices behind it its targets.
//! A device-direct bridge is a [`Host`]: one process, one MQTT session, many devices, each an
//! instance the Store configures with a retained `Config/{instance}` and the bridge's factory builds
//! (multi-device spec, `docs/superpowers/specs/2026-10-10-multi-device-host-design.md`; see
//! [`host`]). The two shapes are one runtime: a `Bridge` is a thin facade over `Host` in its N = 1
//! mode — one fixed instance, named from `--instance`, owned by the process itself (no Config, no
//! `via` tag, `Active/{instance}`), with R1's CONNACK duties and wire — so the session, the
//! lifecycle, the publication, the packet-id budget and the shutdown flush are written once.
//! To learn the API by building a bridge, read `docs/tutorial-writing-a-bridge.md` (its
//! code is `examples/tutorial_bridge.rs`, and its §12, a `Host`, `examples/tutorial_multidevice.rs`);
//! for the command line, the topic grammar, the testing kit and the no-hang guarantees, this crate's
//! `README.md`.
//!
//! What the runtime owns (spec §5), and where:
//!
//! | Area | Module |
//! |---|---|
//! | the topic grammar from `--root` and `--instance`; the subscriptions | [`topics`], [`session`] |
//! | the pump: polls only, one connection per client; an unbounded forward with a backlog episode; MQTT progress bounded | [`pump`] |
//! | every CONNACK: subscribe (the read-back first, its SUBACK awaited), `Active=true`, `Version`, the retained model again, every request and publish within the packet-id budget | [`session`], the host's lifecycle ([`host`]) |
//! | the start-up read-back of the bridge's own retained `State` | [`session`], [`reports`] |
//! | requests: both `Desired` forms, the deadline from the message expiry, `conflict`, retractions, malformed payloads answered on `Error` | [`session`], [`wire`] |
//! | the mailbox: latest per (target, property), momentary FIFO (64) refused while down, reads coalesced, held values dropped at their deadline, unproven writes held again | [`mailbox`] |
//! | the worker: paced connects, proof of life, catch-up with the outage conflict rule, polls, probes | [`worker`] |
//! | reports that never wait; capped `Event` and `Error` queues | [`reports`], [`link`] |
//! | replies on the response topic, with the correlation data | [`replies`] |
//! | outage episodes with the fleet's kind names | [`pump`], [`worker`] |
//! | the process: signals first, the logging start raced against the stop, a bounded end, a panic hook, a watchdog on the device worker | [`process`], [`logging`] |
//! | (a [`Host`]) the instances: `Config/{instance}` and its effective owner, the per-instance bootstrap (S1-S3, SUBACKs by packet id), construction off to the side, stopping and handover, the `via` tags and healing, the sealed batches served round-robin | [`host`], [`session`] |

#![warn(missing_docs)]

// The test seams compile only in this repository's own builds (build.rs; re-gate 3 S6): a deployed
// bridge whose workspace unified the feature in fails here instead of shipping them.
#[cfg(all(feature = "test-seams", not(mqtt_bridge_test_seams)))]
compile_error!(
    "mqtt-bridge-kit's `test-seams` feature is for the Bridge Runtime's conformance suite only: build it from that repository (MQTT_BRIDGE_TEST_SEAMS=1)"
);

mod buffering;
mod completion;
pub mod config;
mod devices;
pub mod driver;
pub mod generations;
pub mod host;
mod lifecycle;
pub mod link;
pub mod logging;
pub mod mailbox;
pub mod managed;
pub mod process;
pub mod pump;
pub mod recovery;
pub mod replies;
pub mod reports;
pub mod session;
pub mod topics;
pub mod wire;
pub mod worker;

#[cfg(test)]
mod test_support;

pub use buffering::BufferLimits;
pub use completion::CommandCompletion;
pub use config::TimingOverrides;
pub use driver::{
    CommandCall, CommandClass, CommandSpec, DeviceDriver, DriverInfo, FailureClass, Feedback, LinkError, OpError, PropertyFailure,
    PropertyMap, Scope, TargetState, Timing, Value, Writes,
};
pub use generations::{ContextualState, RequestContext, RequestKind, RequestPolicy, TargetGenerations};
pub use host::{DeviceConfig, FactoryError, Host};
pub use link::{LinkContext, LinkHandle, ReadBack, Reporter};
pub use managed::{ProcessService, ServiceContext, ServiceTiming};
pub use process::{install_panic_hook, Bridge, Died, Running};
pub use recovery::{
    RecoveryConnection, RecoveryHandle, RecoveryImage, RecoveryOptions, RecoveryReadback, RecoveryReady, RecoveryReceipt, RecoverySnapshot,
};
pub use session::MqttTiming;
pub use wire::Conflict;

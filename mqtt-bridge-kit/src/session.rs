//! The MQTT side (lifted from mqtt_ac's `service.rs`, `mqtt_subscriber.rs` and
//! `mqtt_publisher.rs`; Store no-hang §14.3, §14.8–§14.9).
//!
//! One session per MQTT client: the pump polls in a task of its own and forwards what arrives; the
//! session reads it, posts requests to the device's mailbox — a post never waits — and publishes
//! what [`Reports`] holds. It may wait on rumqttc's request channel: back-pressure, which the
//! pump's polling keeps moving. It never waits on the device, so every CONNACK is read and answered
//! whatever the device is doing (no-hang 3b, review B3).
//!
//! **On every CONNACK** (rumqttc reconnects inside one event loop, and the broker keeps no
//! session): subscribe, then `Active=true`, then the version, then the whole retained model again
//! from the bridge's own state — a clean-start reconnect dropped whatever rumqttc still held, and a
//! broker restarted without persistence lost the retained messages (the fleet's F4). rumqttc's
//! `pending` is never replayed: it comes back in packet-id order, so after a wrap an older value of
//! a topic would follow a newer one.
//!
//! **The start-up read-back:** the process's first CONNACK subscribes to the bridge's own retained
//! `State` before anything else, so the broker delivers it ahead of any retained request, and keeps
//! it as the outage conflict rule's "before" values after a restart (Store kit spec §4.5). Only
//! retained deliveries count: the bridge's own later publishes come back unflagged, and are
//! ignored.
//!
//! **Shutdown:** `Active=false`, then DISCONNECT, polling until it is written — within
//! [`FLUSH_BOUND`]; past it the last will says `Active=false` instead (INFO `bridge_shutdown_cut`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{LastWill, Publish, PublishProperties};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, MqttOptions};
use tokio::sync::watch;
use tracing::{debug, info, Instrument};

use crate::driver::{CommandClass, CommandSpec};
use crate::mailbox::{Mailbox, Posted, ReadKey, StateValue};
use crate::pump::{BrokerOutage, Incoming, Pump, PumpEvent};
use crate::replies::{reply_payload, Answer, ReplyTo};
use crate::reports::{Changes, Model, Outbound, Reports};
use crate::topics::{Inbound, Topics};
use crate::wire::{self, ErrorReport, Reason};

/// The bound on the shutdown's flush: `Active=false` and DISCONNECT, written.
pub const FLUSH_BOUND: Duration = Duration::from_secs(2);

/// The MQTT side's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqttTiming {
    pub keep_alive: Duration,
    /// Between sessions: after one gives up (five failed polls in a row), a fresh client.
    pub retry: Duration,
    /// The pause after a failed poll, before rumqttc's reconnect.
    pub poll_retry: Duration,
    /// A broker outage this long is one WARN (`external_failure`).
    pub warn_after: Duration,
    /// A connection made during an outage that lasts this long ends it.
    pub stable_after: Duration,
}

impl Default for MqttTiming {
    fn default() -> MqttTiming {
        MqttTiming {
            keep_alive: Duration::from_secs(5),
            retry: Duration::from_secs(10),
            poll_retry: Duration::from_secs(1),
            warn_after: Duration::from_secs(30),
            stable_after: Duration::from_secs(5),
        }
    }
}

/// What every session shares, for the life of the bridge.
pub(crate) struct Shared {
    pub topics: Topics,
    pub broker_host: String,
    pub broker_port: u16,
    pub timing: MqttTiming,
    pub version: String,
    pub commands: Vec<CommandSpec>,
    pub mailbox: Arc<Mailbox>,
    pub reports: Arc<Reports>,
    pub outage: Arc<BrokerOutage>,
    /// The read-back's subscription has been made (once per process).
    pub read_back_requested: AtomicBool,
}

/// How a session ended.
enum SessionEnd {
    /// The pump gave up; a new session follows.
    Ended,
    /// The shutdown.
    Shutdown,
}

/// The MQTT side, for the life of the bridge: sessions one after another, until `shutdown` turns
/// true.
pub(crate) async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    loop {
        if let SessionEnd::Shutdown = session(&shared, &mut shutdown).await {
            return;
        }
        debug!(retry_ms = shared.timing.retry.as_millis() as u64, "MQTT session ended; starting a new one");
        // WAIT: session-retry
        tokio::select! {
            () = tokio::time::sleep(shared.timing.retry) => {}
            _ = shutdown.wait_for(|stop| *stop) => return,
        }
    }
}

fn client(shared: &Shared) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let topics = &shared.topics;
    let client_id = format!("{}-{}", topics.root(), topics.instance());
    let mut options = MqttOptions::new(client_id, shared.broker_host.clone(), shared.broker_port);
    let will = LastWill::new(topics.active(), "false", QoS::AtLeastOnce, true, None);
    options.set_keep_alive(shared.timing.keep_alive).set_clean_start(true).set_last_will(will);
    AsyncClient::new(options, 100)
}

/// Outbound publishes carry the current trace's `traceparent`, if a trace is active.
fn properties() -> PublishProperties {
    let mut properties = PublishProperties::default();
    if let Some(traceparent) = tracing_init::traceparent::current() {
        properties.user_properties.push(("traceparent".into(), traceparent));
    }
    properties
}

type Failed = rumqttc::v5::ClientError;

async fn emit(client: &AsyncClient, topic: String, retain: bool, payload: Vec<u8>, properties: PublishProperties) -> Result<(), Failed> {
    // WAIT: mqtt-request
    client.publish_with_properties(topic, QoS::AtLeastOnce, retain, payload, properties).await
}

/// One session: a fresh client and its pump, until the pump gives up or the shutdown.
async fn session(shared: &Shared, shutdown: &mut watch::Receiver<bool>) -> SessionEnd {
    let (client, events) = client(shared);
    // Polling first, so every request below has an event loop draining it.
    let (_pump, mut incoming) = Pump::start(events, shared.outage.clone(), shared.timing.poll_retry);
    // Nothing is published before the first CONNACK's duties are done: the subscriptions come
    // first, and a state published ahead of the read-back's subscription would overwrite the
    // retained state it is there to read.
    let mut announced = false;
    loop {
        // WAIT: session-events
        let step = tokio::select! {
            event = incoming.recv() => Step::Event(event),
            () = shared.reports.reported(), if announced => Step::Reported,
            _ = shutdown.wait_for(|stop| *stop) => Step::Shutdown,
        };
        let connack = matches!(step, Step::Event(Some(PumpEvent::Connected)));
        let work = async {
            match step {
                Step::Event(Some(PumpEvent::Connected)) => connected(shared, &client).await.map(|()| None),
                Step::Event(Some(PumpEvent::Publish(publish))) => {
                    handle(shared, &publish);
                    Ok(None)
                }
                Step::Event(Some(PumpEvent::Disconnected)) => Ok(None),
                Step::Event(Some(PumpEvent::Ended(_)) | None) => Ok(Some(SessionEnd::Ended)),
                Step::Reported => publish_changes(shared, &client, shared.reports.take()).await.map(|()| None),
                Step::Shutdown => Ok(Some(SessionEnd::Shutdown)),
            }
        };
        // A publish waits while the broker stalls; the shutdown does not wait behind it.
        // WAIT: session-work
        let done = tokio::select! {
            done = work => done,
            _ = shutdown.wait_for(|stop| *stop) => Ok(Some(SessionEnd::Shutdown)),
        };
        match done {
            Ok(None) => announced |= connack,
            Ok(Some(SessionEnd::Ended)) => return SessionEnd::Ended,
            Ok(Some(SessionEnd::Shutdown)) => {
                flush(shared, &client, &mut incoming).await;
                return SessionEnd::Shutdown;
            }
            // The event loop is gone (the pump ended): the next event says so.
            Err(e) => debug!(error = %e, "An MQTT request failed: the event loop is gone"),
        }
    }
}

// Transient: one per pass of the session's loop, never stored.
#[allow(clippy::large_enum_variant)]
enum Step {
    Event(Option<PumpEvent>),
    Reported,
    Shutdown,
}

/// The CONNACK duties, in order: the subscriptions (the read-back's first, once per process), then
/// `Active=true`, the version, and the whole retained model.
async fn connected(shared: &Shared, client: &AsyncClient) -> Result<(), Failed> {
    let topics = &shared.topics;
    if !shared.read_back_requested.swap(true, Ordering::SeqCst) {
        // WAIT: mqtt-request
        client.subscribe(topics.state_filter(), QoS::AtLeastOnce).await?;
    }
    for filter in [topics.desired_filter(), topics.command(), topics.config_filter()] {
        // WAIT: mqtt-request
        client.subscribe(filter, QoS::AtLeastOnce).await?;
    }
    emit(client, topics.active(), true, b"true".to_vec(), properties()).await?;
    emit(client, topics.version(), true, shared.version.clone().into_bytes(), properties()).await?;
    republish(shared, client, shared.reports.model()).await
}

/// Everything known, again.
async fn republish(shared: &Shared, client: &AsyncClient, model: Model) -> Result<(), Failed> {
    debug!(targets = model.states.len(), "Publishing the retained model again after a CONNACK");
    if let Some(status) = model.status {
        emit(client, shared.topics.status(), true, status.as_str().as_bytes().to_vec(), properties()).await?;
    }
    for (target, document) in model.states {
        emit(client, shared.topics.state(&target), true, document, properties()).await?;
    }
    Ok(())
}

/// What the device side reported since the last take.
async fn publish_changes(shared: &Shared, client: &AsyncClient, changes: Changes) -> Result<(), Failed> {
    let topics = &shared.topics;
    if let Some(status) = changes.status {
        emit(client, topics.status(), true, status.as_str().as_bytes().to_vec(), properties()).await?;
    }
    for (target, document) in changes.states {
        emit(client, topics.state(&target), true, document, properties()).await?;
    }
    for error in changes.errors {
        emit(client, topics.error(), false, error.encode(), properties()).await?;
    }
    for event in changes.events {
        let mut props = properties();
        props.correlation_data = event.correlation;
        emit(client, event.topic.unwrap_or_else(|| topics.event()), false, event.payload, props).await?;
    }
    Ok(())
}

/// The shutdown's flush: `Active=false` and DISCONNECT, polling until it is written, within
/// [`FLUSH_BOUND`]. Skipped when the broker is out of reach: the last will says it then.
async fn flush(shared: &Shared, client: &AsyncClient, incoming: &mut Incoming) {
    if shared.outage.down() {
        info!(kind = "bridge_shutdown_cut", "The broker is out of reach at shutdown; the last will says Active=false");
        return;
    }
    let flushed = async {
        emit(client, shared.topics.active(), true, b"false".to_vec(), properties()).await?;
        // WAIT: mqtt-request
        client.disconnect().await?;
        loop {
            // WAIT: mqtt-pump-queue
            match incoming.recv().await {
                Some(PumpEvent::Disconnected) | Some(PumpEvent::Ended(_)) | None => return Ok::<(), Failed>(()),
                Some(_) => {}
            }
        }
    };
    // WAIT: shutdown-flush
    match tokio::time::timeout(FLUSH_BOUND, flushed).await {
        Ok(Ok(())) => debug!("Active=false and DISCONNECT written"),
        _ => info!(
            kind = "bridge_shutdown_cut",
            bound_ms = FLUSH_BOUND.as_millis() as u64,
            "The shutdown's flush ran out its bound; the last will says Active=false"
        ),
    }
}

/// An incoming publish, handled without waiting: requests go to the mailbox, what must be answered
/// to the reports.
fn handle(shared: &Shared, publish: &Publish) {
    let topic = String::from_utf8_lossy(&publish.topic).into_owned();
    let traceparent =
        publish.properties.as_ref().and_then(|p| p.user_properties.iter().find(|(k, _)| k == "traceparent").map(|(_, v)| v.clone()));
    let span = tracing::debug_span!("mqtt_request", topic = %topic);
    if let Some(tp) = &traceparent {
        tracing_init::traceparent::set_remote_parent(&span, tp);
    }
    span.in_scope(|| route(shared, &topic, publish));
}

fn route(shared: &Shared, topic: &str, publish: &Publish) {
    let now = Instant::now();
    let properties = publish.properties.as_ref();
    let payload = &publish.payload[..];
    let malformed = |error: String, report: ErrorReport| {
        info!(kind = "validation_rejected", topic, error = %error, "A request was malformed; answered on Error");
        shared.reports.error(ErrorReport { error, ..report });
    };
    let blank = || ErrorReport::new(Reason::Malformed, "");
    match shared.topics.parse(topic) {
        Inbound::State { target } => {
            if publish.retain {
                match wire::decode_state(payload) {
                    Ok(values) => shared.reports.read_back(&target, values),
                    Err(e) => debug!(target = %target, error = %e, "The retained state read back is not a state; ignored"),
                }
            }
        }
        Inbound::DesiredProperty { target, property } => {
            let report = blank().target(Some(&target)).property(&property);
            match wire::decode_desired_property(payload) {
                Err(e) => malformed(e, report),
                Ok(None) => shared.mailbox.retract(&target, &property),
                Ok(Some(value)) => match wire::request_meta(properties, now) {
                    Err(e) => malformed(e, report),
                    Ok(meta) => {
                        let value = StateValue { target, property, value, deadline: meta.deadline, conflict: meta.conflict };
                        shared.mailbox.post_state(value, publish.retain);
                    }
                },
            }
        }
        Inbound::DesiredTarget { target } => {
            let report = blank().target(Some(&target));
            if publish.retain {
                return malformed("a target-level Desired is never retained".into(), report);
            }
            match wire::decode_desired_target(payload).and_then(|values| Ok((values, wire::request_meta(properties, now)?))) {
                Err(e) => malformed(e, report),
                Ok((values, meta)) => {
                    let values = values
                        .into_iter()
                        .map(|(property, value)| StateValue {
                            target: target.clone(),
                            property,
                            value,
                            deadline: meta.deadline,
                            conflict: meta.conflict,
                        })
                        .collect();
                    shared.mailbox.post_target(values);
                }
            }
        }
        Inbound::Command => {
            if publish.retain {
                return malformed("a command is never retained".into(), blank());
            }
            let decoded =
                wire::decode_command(payload).and_then(|(target, call)| Ok((target, call, ReplyTo::from_properties(properties)?)));
            let (target, call, reply) = match decoded {
                Err(e) => return malformed(e, blank()),
                Ok(decoded) => decoded,
            };
            command(
                shared,
                target,
                call,
                reply,
                properties.and_then(|p| p.message_expiry_interval).map(|s| now + Duration::from_secs(u64::from(s))),
            );
        }
        Inbound::Config { target } => match wire::decode_config(payload) {
            Err(e) => malformed(e, blank().target(Some(&target))),
            Ok(config) => {
                shared.mailbox.post_config(&target, config);
            }
        },
        Inbound::Unknown => malformed(format!("`{topic}` is no topic of this bridge's grammar"), blank()),
    }
}

/// A command: the standard `refresh` and `assume`, or one the driver declares.
fn command(shared: &Shared, target: Option<String>, call: crate::driver::CommandCall, reply: Option<ReplyTo>, deadline: Option<Instant>) {
    let answer = |answer: Answer| {
        if let Some(reply) = &reply {
            shared.reports.event(Outbound {
                topic: Some(reply.topic.clone()),
                payload: reply_payload(target.as_deref(), &call.command, &answer),
                correlation: reply.correlation.clone(),
            });
        }
    };
    let refused = |reason: Reason, error: &str| {
        shared.reports.error(ErrorReport::new(reason, error).target(target.as_deref()).command(&call.command));
        answer(Answer::Failed { reason, error: error.to_owned() });
    };
    match call.command.as_str() {
        "refresh" => {
            let posted = shared.mailbox.post_read(ReadKey::Refresh(target.clone()), None, reply.clone());
            if posted == Posted::Dropped {
                answer(Answer::Failed { reason: Reason::Refused, error: "the device is not connected".into() });
            }
        }
        "assume" => match &target {
            Some(t) if !call.params.is_empty() => {
                shared.reports.state(t, call.params.clone(), true);
                answer(Answer::Done(None));
            }
            _ => {
                info!(kind = "validation_rejected", command = "assume", "`assume` names a target and the values to assume");
                refused(Reason::Malformed, "`assume` names a target and the values to assume");
            }
        },
        name => match shared.commands.iter().find(|c| c.name == name).map(|c| c.class) {
            None => {
                info!(command = name, "A command the driver does not declare; answered on Error");
                refused(Reason::Unsupported, "no such command");
            }
            Some(CommandClass::Read) => {
                let key = ReadKey::Command { target: target.clone(), command: name.to_owned() };
                if shared.mailbox.post_read(key, Some(call.clone()), reply.clone()) == Posted::Dropped {
                    answer(Answer::Failed { reason: Reason::Refused, error: "the device is not connected".into() });
                }
            }
            Some(CommandClass::Momentary | CommandClass::WithReply) => {
                if let Posted::Refused(why) = shared.mailbox.post_command(target.clone(), call.clone(), reply.clone(), deadline) {
                    refused(Reason::Refused, why.as_str());
                }
            }
        },
    }
}

/// Spawn the MQTT side on the current runtime.
pub(crate) fn spawn(shared: Arc<Shared>, shutdown: watch::Receiver<bool>, set: &mut tokio::task::JoinSet<()>) {
    set.spawn(run(shared, shutdown).in_current_span());
}

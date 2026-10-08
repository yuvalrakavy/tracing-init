//! The MQTT side (lifted from mqtt_ac's `service.rs`, `mqtt_subscriber.rs` and
//! `mqtt_publisher.rs`; Store no-hang §14.3, §14.8–§14.9).
//!
//! One session per connection, each with a client of its own: the pump polls in a task of its own
//! and forwards what arrives; the session reads it, posts requests to the device's mailbox — a post
//! never waits — and publishes what [`Reports`] holds. It may wait on rumqttc's request channel:
//! back-pressure, which the pump's polling keeps moving, and which the pump bounds (a broker that
//! acknowledges nothing, or stops reading, ends the session). It never waits on the device, so
//! every CONNACK is read and answered whatever the device is doing (no-hang 3b, review B3).
//!
//! **On every CONNACK** (the broker keeps no session): subscribe, then `Active=true`, then the
//! version, then the whole retained model again from the bridge's own state — a broker restarted
//! without persistence lost the retained messages (the fleet's F4). Nothing a previous connection
//! queued goes out: its client went with it. A publish rumqttc refuses is that one message
//! dropped.
//!
//! **The start-up read-back:** the process's first CONNACK subscribes to the bridge's own retained
//! `State` before anything else, and subscribes to its requests only once that subscription is
//! acknowledged (within [`READ_BACK_WAIT`]): the broker queues a subscription's retained messages
//! behind its SUBACK, so the retained `State` is on the one stream ahead of every retained request
//! (review C9). It is kept as the outage conflict rule's "before" values after a restart (Store
//! kit spec §4.5). Only retained deliveries count: the bridge's own later publishes come back
//! unflagged, and are ignored. A session that ends before the SUBACK leaves the read-back to the
//! next.
//!
//! **Shutdown:** `Active=false`, then DISCONNECT, polling until it is written — within
//! [`FLUSH_BOUND`]; past it the last will says `Active=false` instead (INFO `bridge_shutdown_cut`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rumqttc::v5::mqttbytes::v5::{Filter, LastWill, Publish, PublishProperties};
use rumqttc::v5::mqttbytes::QoS;
use rumqttc::v5::{AsyncClient, MqttOptions};
use tokio::sync::watch;
use tracing::{debug, info, warn, Instrument};

use crate::driver::{CommandClass, CommandSpec};
use crate::mailbox::{Check, Mailbox, Posted, ReadKey, StateValue};
use crate::pump::{stall_bound, BrokerOutage, Incoming, Pump, PumpEvent, MAX_CONSECUTIVE_ERRORS};
use crate::replies::{reply_payload, Answer, ReplyTo};
use crate::reports::{Changes, Model, Outbound, Reports};
use crate::topics::{Inbound, Topics};
use crate::wire::{self, ErrorReport, Reason};

/// The bound on the shutdown's flush: `Active=false` and DISCONNECT, written.
pub const FLUSH_BOUND: Duration = Duration::from_secs(2);

/// How long the first CONNACK waits for the read-back's SUBACK before it subscribes to the
/// requests without it (WARN `read_back_incomplete`).
pub const READ_BACK_WAIT: Duration = Duration::from_secs(5);

/// The largest packet the bridge takes, advertised in its CONNECT, so a broker discards a larger
/// message for it rather than send it. rumqttc's default, 10 KiB, is enforced but not advertised:
/// the broker replayed a larger retained `Config` or `Desired` on every subscribe and rumqttc
/// dropped the connection each time — a reconnect loop (review B3). 1 MiB holds the largest
/// payloads the fleet expects (IR code sets, DMX arrays and effects: tens of KiB) many times over,
/// matches the stdio plugin protocol's line cap (spec §7), and bounds what one message can cost.
pub const MAX_PACKET_SIZE: u32 = 1024 * 1024;

/// The test seam's environment variable (with the `test-seams` feature only): see [`panic_for_test`].
pub const TEST_PANIC_TOPIC: &str = "MQTT_BRIDGE_TEST_PANIC_TOPIC";

/// The MQTT side's timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MqttTiming {
    /// MQTT keep-alive (at least 5 s, rumqttc's floor). Twice it bounds how long the broker may
    /// leave a request unacknowledged, and one poll may take ([`crate::pump::stall_bound`]).
    pub keep_alive: Duration,
    /// After `MAX_CONSECUTIVE_ERRORS` failed connects in a row (five), the pause before the next.
    pub retry: Duration,
    /// The pause before a reconnect otherwise. Each connection is a session of its own, with a
    /// fresh client.
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
    /// The read-back's packet id plus one, while its SUBACK is late (0: none): a late SUBACK that
    /// grants it completes the read-back.
    pub read_back_late: std::sync::atomic::AtomicU32,
    /// Whether the driver restores one-way targets from the read-back (`DriverInfo::restore`), and
    /// whether that was done, or said undone, once.
    pub restore: bool,
    pub restored: AtomicBool,
    pub restore_said: AtomicBool,
    /// The request subscriptions whose SUBACK has not come yet: behind the last, every retained
    /// Config and Desired has been read, and a restore can leave them to go first.
    pub subscriptions_pending: std::sync::atomic::AtomicU32,
    /// The read-back's SUBACK granted it, and the retained `State` it replays follow it: the next
    /// SUBACK — the next subscription's, sent after it — comes behind them, and completes the
    /// read-back.
    pub read_back_next: AtomicBool,
}

/// How a session ended.
enum SessionEnd {
    /// Its connection failed, or never came (`connected`: whether the broker accepted it); a new
    /// session follows.
    Ended { connected: bool },
    /// The shutdown.
    Shutdown,
}

/// The MQTT side, for the life of the bridge: sessions one after another, one per connection,
/// until `shutdown` turns true. The pause before the next is `poll_retry`, and `retry` after every
/// `MAX_CONSECUTIVE_ERRORS`th failed connect in a row.
pub(crate) async fn run(shared: Arc<Shared>, mut shutdown: watch::Receiver<bool>) {
    let mut failed: u32 = 0;
    loop {
        let started = Instant::now();
        let connected = match session(&shared, &mut shutdown).await {
            SessionEnd::Shutdown => return,
            SessionEnd::Ended { connected } => connected,
        };
        // A connection lost after it held is the first failure of what follows; a connect refused,
        // or a connection dropped before `stable_after` (two bridges sharing a client id), one more
        // (Claude's re-gate minor 12: every accepted connection reset the count).
        failed = if connected && started.elapsed() >= shared.timing.stable_after { 1 } else { failed + 1 };
        let pause = if failed % MAX_CONSECUTIVE_ERRORS == 0 { shared.timing.retry } else { shared.timing.poll_retry };
        debug!(pause_ms = pause.as_millis() as u64, failed, "MQTT session ended; starting a new one");
        // WAIT: session-retry
        tokio::select! {
            () = tokio::time::sleep(pause) => {}
            _ = shutdown.wait_for(|stop| *stop) => return,
        }
    }
}

fn client(shared: &Shared) -> (AsyncClient, rumqttc::v5::EventLoop) {
    let topics = &shared.topics;
    let client_id = format!("{}-{}", topics.root(), topics.instance());
    let mut options = MqttOptions::new(client_id, shared.broker_host.clone(), shared.broker_port);
    let will = LastWill::new(topics.active(), "false", QoS::AtLeastOnce, true, None);
    options.set_keep_alive(shared.timing.keep_alive).set_clean_start(true).set_last_will(will).set_max_packet_size(Some(MAX_PACKET_SIZE));
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

/// Publish one message. A refusal — a topic rumqttc does not publish to, or the event loop gone —
/// is that one message dropped, said at DEBUG; the caller goes on with the rest (review B2: the
/// first refusal ended the CONNACK duties, and the session never published a report again).
async fn emit(client: &AsyncClient, topic: String, retain: bool, payload: Vec<u8>, properties: PublishProperties) {
    let refused_topic = topic.clone();
    // WAIT: mqtt-request
    if let Err(e) = client.publish_with_properties(topic, QoS::AtLeastOnce, retain, payload, properties).await {
        debug!(topic = %refused_topic, error = %e, "An MQTT publish was refused; this message is dropped");
    }
}

/// One session: a fresh client and its pump, for one connection, until it ends or the shutdown.
async fn session(shared: &Shared, shutdown: &mut watch::Receiver<bool>) -> SessionEnd {
    let (client, events) = client(shared);
    // Polling first, so every request below has an event loop draining it.
    // When the connection is lost, what the bridge knew then is the conflict rule's "before" values
    // for what the broker replays on the next one (review B1).
    let reports = shared.reports.clone();
    let (_pump, mut incoming) =
        Pump::start(events, shared.outage.clone(), stall_bound(shared.timing.keep_alive), move || reports.broker_lost());
    // Nothing is published before the first CONNACK's duties are done: the subscriptions come
    // first, and a state published ahead of the read-back's subscription would overwrite the
    // retained state it is there to read.
    let mut announced = false;
    // Whether the broker accepted this session's connection.
    let mut accepted = false;
    loop {
        // WAIT: session-events
        // The stop first: its flush publishes what is pending anyway (re-gate R15). Then what is
        // pending before the next read: `reported` fires only with something queued, so it cannot
        // starve the reads, while the reads polled first would drain a whole backlog before a
        // publish and displace its replies past the event cap (re-gate 3 S2, RG3-3).
        let step = tokio::select! {
            biased;
            _ = shutdown.wait_for(|stop| *stop) => Step::Shutdown,
            () = shared.reports.reported(), if announced => Step::Reported,
            event = incoming.recv() => Step::Event(event),
        };
        let connack = matches!(step, Step::Event(Some(PumpEvent::Connected)));
        accepted |= connack;
        if let Step::Event(Some(PumpEvent::SubAck { pkid, granted })) = &step {
            let _ = shared.subscriptions_pending.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
            if shared.read_back_next.swap(false, Ordering::SeqCst) {
                // The next subscription's SUBACK, behind the read-back's replay.
                read_back_complete(shared);
            } else if shared.read_back_late.load(Ordering::SeqCst) == u32::from(*pkid) + 1 {
                // The read-back's SUBACK, past its wait: its replay follows it, and the SUBACKs of
                // the subscriptions made after it — sent after it, answered in order — come behind
                // the replay; the next completes the read-back.
                shared.read_back_late.store(0, Ordering::SeqCst);
                if *granted {
                    info!("The read-back's SUBACK came late; the read-back completes behind its replay");
                    shared.read_back_next.store(true, Ordering::SeqCst);
                }
            }
        }
        restore_when_ready(shared);
        let work = async {
            match step {
                Step::Event(Some(PumpEvent::Connected)) => connected(shared, &client, &mut incoming).await.map(|goes_on| {
                    if goes_on {
                        None
                    } else {
                        Some(SessionEnd::Ended { connected: false })
                    }
                }),
                Step::Event(Some(PumpEvent::Publish(publish, received))) => {
                    handle(shared, &publish, received);
                    Ok(None)
                }
                Step::Event(Some(PumpEvent::Disconnected | PumpEvent::SubAck { .. } | PumpEvent::Subscribing(_))) => Ok(None),
                Step::Event(Some(PumpEvent::Ended(_))) => Ok(Some(SessionEnd::Ended { connected: false })),
                // The pump went without a word — it panicked — so it never said the connection was
                // lost: the broker snapshot is taken here (Claude's re-gate minor 14).
                Step::Event(None) => {
                    if accepted {
                        shared.reports.broker_lost();
                    }
                    Ok(Some(SessionEnd::Ended { connected: false }))
                }
                Step::Reported => {
                    publish_changes(shared, &client, shared.reports.take()).await;
                    Ok(None)
                }
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
            Ok(Some(SessionEnd::Ended { .. })) => {
                // A late SUBACK cannot come on another connection; a replay cut short is what
                // there is.
                shared.read_back_late.store(0, Ordering::SeqCst);
                if shared.read_back_next.swap(false, Ordering::SeqCst) {
                    read_back_incomplete(shared);
                }
                return SessionEnd::Ended { connected: accepted };
            }
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
async fn connected(shared: &Shared, client: &AsyncClient, incoming: &mut Incoming) -> Result<bool, Failed> {
    let topics = &shared.topics;
    if !shared.read_back_requested.load(Ordering::SeqCst) {
        // WAIT: mqtt-request
        client.subscribe(topics.state_filter(), QoS::AtLeastOnce).await?;
        match read_back_acknowledged(shared, incoming).await {
            ReadBack::Ended => return Ok(false),
            // Complete once its replay is in: at the next SUBACK, behind it.
            ReadBack::Granted => shared.read_back_next.store(true, Ordering::SeqCst),
            ReadBack::Refused => {
                read_back_incomplete(shared);
                warn!(
                    kind = "read_back_incomplete",
                    refused = true,
                    "The broker refused the read-back's subscription; subscribing to requests without it — a replay may meet no \"before\" values"
                );
            }
            ReadBack::Late(pkid) => {
                shared.read_back_late.store(pkid.map_or(0, |p| u32::from(p) + 1), Ordering::SeqCst);
                read_back_incomplete(shared);
                warn!(
                    kind = "read_back_incomplete",
                    waited_ms = READ_BACK_WAIT.as_millis() as u64,
                    "The broker did not acknowledge the read-back's subscription in time; subscribing to requests without it — a replay may meet no \"before\" values"
                );
            }
        }
        shared.read_back_requested.store(true, Ordering::SeqCst);
    }
    // Configs first, so a target's retained setup is queued ahead of its retained values (review
    // M4). Target-level writes and commands are never retained: subscribed retain-as-published, a
    // live retained one keeps its flag and is refused like a replayed one. Property writes are
    // retained by design: their subscription clears the flag on a live delivery, so only a replay
    // arrives flagged (review C10).
    let requests = [
        (topics.config_filter(), false),
        (topics.desired_target_filter(), true),
        (topics.desired_property_filter(), false),
        (topics.command(), true),
    ];
    shared.subscriptions_pending.store(requests.len() as u32, Ordering::SeqCst);
    for (filter, retain_as_published) in requests {
        let filter = Filter { preserve_retain: retain_as_published, ..Filter::new(filter, QoS::AtLeastOnce) };
        // WAIT: mqtt-request
        client.subscribe_many(vec![filter]).await?;
    }
    emit(client, topics.active(), true, b"true".to_vec(), properties()).await;
    emit(client, topics.version(), true, shared.version.clone().into_bytes(), properties()).await;
    republish(shared, client, shared.reports.model()).await;
    Ok(true)
}

/// What came of the read-back's subscription.
enum ReadBack {
    /// Its SUBACK granted it: every retained State has been read.
    Granted,
    /// Its SUBACK refused it (an ACL): there is no read-back.
    Refused,
    /// No SUBACK within [`READ_BACK_WAIT`]; the subscription's packet id, if it was written.
    Late(Option<u16>),
    /// The session ended.
    Ended,
}

/// The read-back's SUBACK — told by its packet id — within [`READ_BACK_WAIT`], handling what
/// arrives meanwhile.
async fn read_back_acknowledged(shared: &Shared, incoming: &mut Incoming) -> ReadBack {
    let deadline = Instant::now() + READ_BACK_WAIT;
    let mut read_back: Option<u16> = None;
    loop {
        // WAIT: read-back-suback
        match tokio::time::timeout_at(deadline.into(), incoming.recv()).await {
            Err(_) => return ReadBack::Late(read_back),
            // The read-back's is the session's first SUBSCRIBE.
            Ok(Some(PumpEvent::Subscribing(pkid))) => {
                read_back.get_or_insert(pkid);
            }
            Ok(Some(PumpEvent::SubAck { pkid, granted })) if Some(pkid) == read_back => {
                return if granted { ReadBack::Granted } else { ReadBack::Refused };
            }
            Ok(Some(PumpEvent::SubAck { .. } | PumpEvent::Connected | PumpEvent::Disconnected)) => {}
            Ok(Some(PumpEvent::Publish(publish, received))) => handle(shared, &publish, received),
            Ok(Some(PumpEvent::Ended(_)) | None) => return ReadBack::Ended,
        }
    }
}

/// Everything known, again. A message refused is dropped alone.
async fn republish(shared: &Shared, client: &AsyncClient, model: Model) {
    debug!(targets = model.states.len(), "Publishing the retained model again after a CONNACK");
    if let Some(status) = model.status {
        emit(client, shared.topics.status(), true, status.as_str().as_bytes().to_vec(), properties()).await;
    }
    for (target, document) in model.states {
        emit(client, shared.topics.state(&target), true, document, properties()).await;
    }
}

/// What the device side reported since the last take. A message refused is dropped alone.
async fn publish_changes(shared: &Shared, client: &AsyncClient, changes: Changes) {
    let topics = &shared.topics;
    if let Some(status) = changes.status {
        emit(client, topics.status(), true, status.as_str().as_bytes().to_vec(), properties()).await;
    }
    for (target, document) in changes.states {
        emit(client, topics.state(&target), true, document, properties()).await;
    }
    for error in changes.errors {
        emit(client, topics.error(), false, error.encode(), properties()).await;
    }
    for event in changes.events {
        let mut props = properties();
        props.correlation_data = event.correlation;
        emit(client, event.topic.unwrap_or_else(|| topics.event()), false, event.payload, props).await;
    }
}

/// The shutdown's flush: `Active=false` and DISCONNECT, polling until it is written, within
/// [`FLUSH_BOUND`]. Skipped when the broker is out of reach: the last will says it then.
async fn flush(shared: &Shared, client: &AsyncClient, incoming: &mut Incoming) {
    if shared.outage.down() {
        info!(kind = "bridge_shutdown_cut", "The broker is out of reach at shutdown; the last will says Active=false");
        return;
    }
    let flushed = async {
        // What still waits for the device will not run: refused, on Error and on its response
        // topic; then what is pending — Status, Errors, replies — then the last words (re-gate
        // R15). Here, in the session's own flush, so nothing else publishes them first by chance.
        crate::worker::report_refused(&shared.reports, shared.mailbox.disconnected(), STOPPED_BEFORE);
        publish_changes(shared, client, shared.reports.take()).await;
        emit(client, shared.topics.active(), true, b"false".to_vec(), properties()).await;
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

/// An incoming publish, received at `received`, handled without waiting: requests go to the
/// mailbox, what must be answered to the reports.
fn handle(shared: &Shared, publish: &Publish, received: Instant) {
    let topic = String::from_utf8_lossy(&publish.topic).into_owned();
    panic_for_test(&topic);
    let traceparent =
        publish.properties.as_ref().and_then(|p| p.user_properties.iter().find(|(k, _)| k == "traceparent").map(|(_, v)| v.clone()));
    let span = tracing::debug_span!("mqtt_request", topic = %topic);
    if let Some(tp) = &traceparent {
        tracing_init::traceparent::set_remote_parent(&span, tp);
    }
    span.in_scope(|| route(shared, &topic, publish, received));
}

/// `now` is when the publish arrived: every deadline counts from it.
fn route(shared: &Shared, topic: &str, publish: &Publish, now: Instant) {
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
                    Ok(values) => {
                        let assumed = serde_json::from_slice::<serde_json::Value>(payload)
                            .ok()
                            .and_then(|v| v.get("assumed").and_then(serde_json::Value::as_bool))
                            .unwrap_or(false);
                        shared.reports.read_back(&target, values, assumed)
                    }
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
                        // A replay's "before" basis, as of its arrival (re-gate R2).
                        let basis = publish.retain.then(|| shared.reports.broker_basis(&target, &property));
                        let value = StateValue {
                            target,
                            property,
                            value,
                            deadline: meta.deadline,
                            conflict: meta.conflict,
                            check: Check::default(),
                            basis,
                        };
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
                            check: Check::default(),
                            basis: None,
                        })
                        .collect();
                    shared.mailbox.post_target(values);
                }
            }
        }
        Inbound::Command => {
            let reply = ReplyTo::from_properties(properties, &shared.topics);
            // A command refused as it arrives still answers on its response topic, if it named
            // one the bridge can publish to (review C13).
            let refuse = |error: String| {
                if let Ok(Some(reply)) = &reply {
                    let (target, command) = wire::command_names(payload);
                    shared.reports.event(Outbound {
                        topic: Some(reply.topic.clone()),
                        payload: reply_payload(
                            target.as_deref(),
                            &command,
                            &Answer::Failed { reason: Reason::Malformed, error: error.clone() },
                        ),
                        correlation: reply.correlation.clone(),
                    });
                }
                malformed(error, blank());
            };
            if publish.retain {
                return refuse("a command is never retained".into());
            }
            let reply = match &reply {
                Err(e) => return malformed(e.clone(), blank()),
                Ok(reply) => reply.clone(),
            };
            let (target, call) = match wire::decode_command(payload) {
                Err(e) => return refuse(e),
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
    // Past its expiry when the session reads it — it waited behind a stalled broker — a command
    // is refused, whatever its class: `assume` and the reads included (review R9).
    if deadline.is_some_and(|d| Instant::now() >= d) {
        debug!(command = %call.command, "A command past its expiry when read; refused");
        return refused(Reason::Refused, "the command reached its expiry before the bridge read it; not executed");
    }
    match call.command.as_str() {
        "refresh" => {
            let posted = shared.mailbox.post_read(ReadKey::Refresh(target.clone()), None, reply.clone(), deadline);
            if posted == Posted::Dropped {
                refused(Reason::Refused, "the device is not connected");
            }
        }
        "assume" => match &target {
            // The Store's input: a property that cannot stand as a topic segment is malformed, not
            // a driver's contract violation (Claude's re-gate minor 6).
            Some(_) if call.params.keys().any(|p| !crate::topics::valid_segment(p)) => {
                info!(kind = "validation_rejected", command = "assume", "`assume` names a property that cannot stand as a topic segment");
                refused(Reason::Malformed, "`assume` names a property that cannot stand as a topic segment");
            }
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
                if shared.mailbox.post_read(key, Some(call.clone()), reply.clone(), deadline) == Posted::Dropped {
                    refused(Reason::Refused, "the device is not connected");
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

/// The test seam for a panicking session task (with the `test-seams` feature only; a build without it has none): when
/// `MQTT_BRIDGE_TEST_PANIC_TOPIC` names a topic, a publish on it panics the session. Unset, it does
/// nothing.
#[cfg(feature = "test-seams")]
fn panic_for_test(topic: &str) {
    static SEAM: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    if SEAM.get_or_init(|| std::env::var(TEST_PANIC_TOPIC).ok()).as_deref() == Some(topic) {
        panic!("the test seam's panic, on a publish on {topic}");
    }
}

#[cfg(not(feature = "test-seams"))]
fn panic_for_test(_: &str) {}

/// How long a restored value may wait for the device: an AC's `RequestExpiry` (kit spec, Q3).
pub const RESTORE_EXPIRY: Duration = Duration::from_secs(3600);

/// The read-back is complete. With restore on, once per process: each value the previous run
/// assumed is posted as an ordinary held request — `RequestWins`, within [`RESTORE_EXPIRY`] — but
/// a key a request was posted for since the start is left to it; from then on the mailbox's rules
/// apply as to any request (re-gate S1: the restore had machinery of its own, and defects with it).
fn read_back_complete(shared: &Shared) {
    shared.reports.read_back_complete();
    restore_when_ready(shared);
}

/// The restore, once the read-back is complete and every request subscription is acknowledged —
/// behind the last SUBACK every retained Config and Desired has been posted, so the configs go
/// first and a newer request keeps its key (re-gate S1).
fn restore_when_ready(shared: &Shared) {
    if !shared.restore || !shared.reports.read_back_is_complete() || shared.subscriptions_pending.load(Ordering::SeqCst) > 0 {
        return;
    }
    if shared.restored.swap(true, Ordering::SeqCst) {
        return;
    }
    let values: Vec<StateValue> = shared
        .reports
        .read_back_assumed()
        .into_iter()
        .flat_map(|(target, values)| {
            values.into_iter().map(move |(property, value)| StateValue {
                target: target.clone(),
                property,
                value,
                deadline: None,
                conflict: crate::wire::Conflict::RequestWins,
                check: Check::default(),
                basis: None,
            })
        })
        .collect();
    let posted = shared.mailbox.post_restore(values, Instant::now() + RESTORE_EXPIRY);
    info!(values = posted, "Restoring one-way targets from the read-back");
}

/// The read-back is incomplete. With restore on, nothing is restored unless it completes after
/// all: said once, and nothing waits for it.
fn read_back_incomplete(shared: &Shared) {
    shared.reports.read_back_incomplete();
    if shared.restore && !shared.restored.load(Ordering::SeqCst) && !shared.restore_said.swap(true, Ordering::SeqCst) {
        warn!(kind = "restore_incomplete", "The read-back is incomplete: no one-way target is restored unless it completes");
    }
}

/// What a stop refuses: a command still waiting for the device.
const STOPPED_BEFORE: &str = "the bridge stopped before it ran";

/// Spawn the MQTT side on the current runtime.
pub(crate) fn spawn(shared: Arc<Shared>, shutdown: watch::Receiver<bool>, set: &mut tokio::task::JoinSet<()>) {
    set.spawn(run(shared, shutdown).in_current_span());
}

#[cfg(test)]
mod tests;

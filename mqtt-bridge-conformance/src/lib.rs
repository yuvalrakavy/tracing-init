//! The Bridge Runtime's conformance suite (Bridge Runtime spec §9): one black-box suite that plays
//! the Store's side over MQTT — `FakeBroker`, on 127.0.0.1, never the real broker — against a
//! bridge driving a scriptable fake device ([`device`]). The tests under `tests/` are the
//! scenarios; a [`Harness`] starts a bridge in-process, and the `fake-bridge` binary runs one as a
//! process for the process scenarios.
//!
//! Every payload a bridge publishes is checked against its lane's JSON Schema
//! (`schemas/`, [`schema`]) when a harness stops, with the lanes' retain flags and QoS.

pub mod capture;
pub mod device;
pub mod schema;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use mqtt_bridge_kit::topics::Topics;
use mqtt_bridge_kit::{Bridge, DriverInfo, MqttTiming, Running, TimingOverrides};
pub use mqtt_test_broker::{BrokerEvent, FakeBroker, QoS, Received, SendOptions};
use serde_json::Value;

pub use capture::{Capture, Record};
pub use device::{fake_device, Answer, Call, Device, FakeDevice, Helper, Op};

/// The bound on any one wait of a scenario: a regression fails the test, never hangs the suite.
pub const BOUND: Duration = Duration::from_secs(20);

/// The root every scenario's bridge uses.
pub const ROOT: &str = "Conf";

/// The driver timing scenarios run with: short, so an outage plays out in a second or two.
pub fn quick_timing() -> TimingOverrides {
    TimingOverrides {
        connect: Some(Duration::from_millis(1000)),
        operation: Some(Duration::from_millis(500)),
        retry: Some(Duration::from_millis(300)),
        outage_warn: Some(Duration::from_secs(30)),
        poll: Some(None),
        probe_interval: Some(Duration::from_secs(30)),
        probe_deadline: Some(Duration::from_millis(500)),
    }
}

/// The MQTT timing scenarios run with.
pub fn quick_mqtt() -> MqttTiming {
    MqttTiming {
        keep_alive: Duration::from_secs(5),
        retry: Duration::from_millis(500),
        poll_retry: Duration::from_millis(100),
        warn_after: Duration::from_secs(30),
        stable_after: Duration::from_secs(5),
    }
}

/// A request's MQTT v5 metadata, as the Store sends it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Req {
    /// The message expiry, in seconds (the Store's `RequestExpiry`).
    pub expiry: Option<u32>,
    /// The user property `conflict` (the Store's `OutageConflict`).
    pub conflict: Option<&'static str>,
}

impl Req {
    fn options(&self) -> SendOptions {
        let mut options = SendOptions { message_expiry: self.expiry, ..SendOptions::default() };
        if let Some(conflict) = self.conflict {
            options = options.user_property("conflict", conflict);
        }
        options
    }
}

/// A unique instance name for a scenario.
pub fn instance_name(name: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!("{name}-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::SeqCst))
}

/// One bridge in-process, its broker, its fake device and its log.
pub struct Harness {
    pub broker: FakeBroker,
    pub device: Device,
    pub topics: Topics,
    pub log: Capture,
    running: Option<Running>,
}

impl Harness {
    /// A bridge of `info`'s driver, with `timing`, once it has subscribed.
    pub async fn start(name: &str, info: DriverInfo, timing: TimingOverrides) -> Harness {
        Harness::start_with(name, info, timing, |_, _, _| {}).await
    }

    /// The same, after `prepare` has set the broker and the device up (retained messages from
    /// before the bridge started, a device that starts down…).
    pub async fn start_with(
        name: &str,
        info: DriverInfo,
        timing: TimingOverrides,
        prepare: impl FnOnce(&FakeBroker, &Topics, &Device),
    ) -> Harness {
        let broker = FakeBroker::start().await;
        broker.cap_connections(50);
        let instance = instance_name(name);
        let topics = Topics::new(ROOT, &instance).expect("the scenario's topics");
        let (driver, device) = fake_device(info);
        device.cap_connects(40);
        prepare(&broker, &topics, &device);
        let log = Capture::start(&instance);
        let running = Bridge::new(ROOT)
            .instance(&instance)
            .broker(&broker.address())
            .timing(timing)
            .mqtt_timing(quick_mqtt())
            .version("conformance")
            .start(driver)
            .expect("the bridge starts");
        let harness = Harness { broker, device, topics, log, running: Some(running) };
        let command = harness.topics.command();
        assert!(harness.broker.wait_for_subscription(&command, BOUND).await, "the bridge never subscribed to {command}");
        harness
    }

    pub fn instance(&self) -> &str {
        self.topics.instance()
    }

    /// Wait until `ready` holds, polling, up to `within`. Returns whether it held.
    pub async fn eventually(&self, within: Duration, mut ready: impl FnMut(&Harness) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if ready(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Hold for `d`, letting the bridge work.
    pub async fn settle(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }

    /// The Store's write of one property: published retained (kept at the broker) and delivered
    /// live.
    pub fn desire(&self, target: &str, property: &str, value: Value, req: Req) -> bool {
        let topic = self.topics.desired(target, property);
        let payload = value.to_string();
        self.broker.retain(&topic, payload.clone(), &req.options().retained());
        self.broker.send_with(&topic, payload, &req.options())
    }

    /// The Store's `desire_many`: the target message, then each property topic, retained.
    pub fn desire_many(&self, target: &str, values: Value, req: Req) -> bool {
        let sent = self.broker.send_with(&self.topics.desired_target(target), values.to_string(), &req.options());
        if let Value::Object(map) = &values {
            for (property, value) in map {
                self.desire(target, property, value.clone(), req);
            }
        }
        sent
    }

    /// The Store's retraction of a property's request.
    pub fn retract(&self, target: &str, property: &str) -> bool {
        let topic = self.topics.desired(target, property);
        self.broker.retain(&topic, "", &SendOptions::default());
        self.broker.send_with(&topic, "", &SendOptions::default())
    }

    /// A command, with a response topic and correlation data when `reply` is given.
    pub fn command(&self, payload: Value, reply: Option<(&str, &[u8])>) -> bool {
        let mut options = SendOptions::default().expiry(30);
        if let Some((topic, correlation)) = reply {
            options = options.response_topic(topic).correlation_data(correlation.to_vec());
        }
        self.broker.send_with(&self.topics.command(), payload.to_string(), &options)
    }

    /// A raw payload on `topic`, live.
    pub fn send_raw(&self, topic: &str, payload: &[u8]) -> bool {
        self.broker.send_with(topic, payload.to_vec(), &SendOptions::default())
    }

    /// A target's config, retained and live.
    pub fn config(&self, target: &str, config: Value) -> bool {
        let topic = self.topics.config(target);
        self.broker.retain(&topic, config.to_string(), &SendOptions::default());
        self.broker.send_with(&topic, config.to_string(), &SendOptions::default())
    }

    /// Every `Error` the bridge published, as JSON.
    pub fn errors(&self) -> Vec<Value> {
        self.broker.received_on(&self.topics.error()).iter().filter_map(|r| serde_json::from_slice(&r.payload).ok()).collect()
    }

    /// The `Error`s naming `reason`.
    pub fn errors_for(&self, reason: &str) -> Vec<Value> {
        self.errors().into_iter().filter(|e| e["reason"] == reason).collect()
    }

    /// A target's latest published `State`.
    pub fn state(&self, target: &str) -> Option<Value> {
        self.broker.received_on(&self.topics.state(target)).last().and_then(|r| serde_json::from_slice(&r.payload).ok())
    }

    /// The latest published `Status`.
    pub fn status(&self) -> Option<String> {
        self.broker.received_on(&self.topics.status()).last().map(|r| String::from_utf8_lossy(&r.payload).into_owned())
    }

    /// What the bridge published on `topic`.
    pub fn published(&self, topic: &str) -> Vec<Received> {
        self.broker.received_on(topic)
    }

    /// The bridge's running handle.
    pub fn running(&mut self) -> &mut Running {
        self.running.as_mut().expect("the bridge is running")
    }

    /// Check every publish so far against its lane: its schema, its retain flag, QoS 1, and no
    /// publish on a lane the Store owns.
    pub fn check_lanes(&self) {
        let t = &self.topics;
        let prefix = |lane: &str| format!("{}/{lane}/{}", t.root(), t.instance());
        for r in self.broker.received() {
            let topic = r.topic.as_str();
            let (lane, retained) = if topic == t.active() {
                ("active", true)
            } else if topic == t.status() {
                ("status", true)
            } else if topic == t.version() {
                ("version", true)
            } else if topic.starts_with(&format!("{}/", prefix("State"))) {
                ("state", true)
            } else if topic == t.error() {
                ("error", false)
            } else if topic == t.event() {
                let reply = serde_json::from_slice::<Value>(&r.payload).is_ok_and(|v| v["kind"] == "reply");
                (if reply { "reply" } else { "event" }, false)
            } else if ["Desired", "Command", "Config"].iter().any(|lane| topic.starts_with(&prefix(lane))) {
                panic!("the bridge published on a lane the Store owns: {topic}");
            } else {
                // A command's own response topic.
                ("reply", false)
            };
            assert_eq!(r.qos, QoS::AtLeastOnce, "{topic} was not published at QoS 1");
            assert_eq!(r.retain, retained, "{topic}: retained is {}, its lane says {retained}", r.retain);
            let instance = schema::instance(lane, &r.payload).unwrap_or_else(|e| panic!("{topic}: {e}"));
            if let Err(e) = schema::check(lane, &instance) {
                panic!("{topic} does not conform to its lane's schema: {e}; the payload: {instance}");
            }
        }
    }

    /// Stop the bridge (bounded), keeping the harness to look at what it left. Returns whether it
    /// stopped in time.
    pub async fn stop_bridge(&mut self) -> bool {
        let running = self.running.take().expect("the bridge is running");
        tokio::time::timeout(BOUND, running.stop()).await.unwrap_or(false)
    }

    /// Stop the bridge (bounded) and check its lanes. Returns whether it stopped in time.
    pub async fn stop(mut self) -> bool {
        let stopped = self.stop_bridge().await;
        self.check_lanes();
        stopped
    }
}

/// A `DriverInfo` with the given writes and feedback, and the scenarios' commands: `pulse`
/// (momentary), `learn` (with a reply) and `status` (a read).
pub fn info(writes: mqtt_bridge_kit::Writes, feedback: mqtt_bridge_kit::Feedback) -> DriverInfo {
    use mqtt_bridge_kit::{CommandClass, CommandSpec};
    DriverInfo {
        writes,
        feedback,
        commands: vec![
            CommandSpec::new("pulse", CommandClass::Momentary),
            CommandSpec::new("learn", CommandClass::WithReply),
            CommandSpec::new("status", CommandClass::Read),
        ],
        timing: mqtt_bridge_kit::Timing::default(),
    }
}

/// The usual driver: acknowledged writes, observed feedback.
pub fn acknowledged() -> DriverInfo {
    info(mqtt_bridge_kit::Writes::Acknowledged, mqtt_bridge_kit::Feedback::Observed)
}

//! The multi-device host's harness (multi-device spec §7): a fake factory ([`Fleet`]) that builds a
//! scriptable fake device per instance, and a [`HostHarness`] that runs a [`Host`] in-process
//! against a [`FakeBroker`] for several clients — the Store's side (each instance's
//! `Config/{instance}`, requests, commands), and a second process of the same kind when a
//! scenario needs one.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mqtt_bridge_kit::topics::Topics;
use mqtt_bridge_kit::{DeviceConfig, DriverInfo, FactoryError, Host, PropertyMap, Running, Value};
use mqtt_test_broker::{BrokerEvent, FakeBroker, QoS, Received, SendOptions};
use serde_json::json;

use crate::{bound, fake_device, quick_mqtt, quick_timing, schema, Capture, Device, FakeDevice, Req, ROOT};

/// The kind every scenario's host is.
pub const KIND: &str = "fakekind";

/// How the factory answers one build, in place of building a fake device at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// A fake device.
    Ok,
    /// `FactoryError::Malformed`: the driver fields are invalid.
    Malformed,
    /// `FactoryError::Unusable`: no driver can be built from them now.
    Unusable,
    /// The factory blocks its thread this long, then builds (a factory that waits for its device).
    Block(Duration),
}

type Prepare = Arc<dyn Fn(&Device) + Send + Sync>;

#[derive(Default)]
struct FleetState {
    info: Option<DriverInfo>,
    prepare: HashMap<String, Prepare>,
    infos: HashMap<String, DriverInfo>,
    script: HashMap<String, VecDeque<Build>>,
    built: HashMap<String, Vec<Device>>,
    configs: HashMap<String, Vec<PropertyMap>>,
}

/// The fake factory: per instance, a fake device made ready by the scenario (`prepare`), and the
/// outcomes of its next builds (`script`); and what it built, with the Config each was built from.
#[derive(Clone, Default)]
pub struct Fleet {
    state: Arc<Mutex<FleetState>>,
}

impl Fleet {
    /// A factory whose devices declare `info`.
    pub fn new(info: DriverInfo) -> Fleet {
        let fleet = Fleet::default();
        fleet.locked().info = Some(info);
        fleet
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, FleetState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Run `prepare` on each device built for `instance`, before the host gets it.
    pub fn prepare(&self, instance: &str, prepare: impl Fn(&Device) + Send + Sync + 'static) {
        self.locked().prepare.insert(instance.to_owned(), Arc::new(prepare));
    }

    /// The devices built for `instance` declare `info` instead.
    pub fn info_for(&self, instance: &str, info: DriverInfo) {
        self.locked().infos.insert(instance.to_owned(), info);
    }

    /// The outcomes of `instance`'s next builds, in order; past them, builds succeed.
    pub fn script(&self, instance: &str, builds: &[Build]) {
        self.locked().script.entry(instance.to_owned()).or_default().extend(builds.iter().copied());
    }

    /// The latest device built for `instance`.
    pub fn device(&self, instance: &str) -> Option<Device> {
        self.locked().built.get(instance).and_then(|d| d.last().cloned())
    }

    /// Every device built for `instance`, oldest first.
    pub fn devices(&self, instance: &str) -> Vec<Device> {
        self.locked().built.get(instance).cloned().unwrap_or_default()
    }

    /// How many builds `instance` has had, failed ones included.
    pub fn builds(&self, instance: &str) -> usize {
        self.locked().configs.get(instance).map_or(0, Vec::len)
    }

    /// The driver fields of each build of `instance`, in order.
    pub fn configs(&self, instance: &str) -> Vec<PropertyMap> {
        self.locked().configs.get(instance).cloned().unwrap_or_default()
    }

    /// The factory to give a host.
    pub fn factory(&self) -> impl Fn(&DeviceConfig) -> Result<FakeDevice, FactoryError> + Send + Sync + 'static {
        let fleet = self.clone();
        move |config: &DeviceConfig| fleet.build(config)
    }

    fn build(&self, config: &DeviceConfig) -> Result<FakeDevice, FactoryError> {
        let instance = config.instance().to_owned();
        let (build, info, prepare) = {
            let mut state = self.locked();
            state.configs.entry(instance.clone()).or_default().push(config.fields().clone());
            let build = state.script.get_mut(&instance).and_then(VecDeque::pop_front).unwrap_or(Build::Ok);
            let info = state.infos.get(&instance).cloned().or_else(|| state.info.clone()).unwrap_or_default();
            (build, info, state.prepare.get(&instance).cloned())
        };
        match build {
            Build::Malformed => return Err(FactoryError::Malformed("scripted: the fields are invalid".into())),
            Build::Unusable => return Err(FactoryError::Unusable("scripted: no driver can be built".into())),
            Build::Block(d) => std::thread::sleep(d),
            Build::Ok => {}
        }
        let (driver, device) = fake_device(info);
        device.cap_connects(40);
        if let Some(prepare) = prepare {
            prepare(&device);
        }
        self.locked().built.entry(instance).or_default().push(device);
        Ok(driver)
    }
}

/// Run `scenario` within `deadline`, its outer bound: a regression fails the test, never hangs it.
pub async fn within<T>(deadline: Duration, scenario: impl Future<Output = T>) -> T {
    match tokio::time::timeout(deadline, scenario).await {
        Ok(done) => done,
        Err(_) => panic!("the scenario ran past its outer deadline of {} s", deadline.as_secs()),
    }
}

/// One host in-process, against a broker for several clients, with its fake factory and its log.
pub struct HostHarness {
    pub broker: Arc<FakeBroker>,
    /// This process's bridge id.
    pub bridge: String,
    pub fleet: Fleet,
    pub log: Capture,
    running: Option<Running>,
}

impl HostHarness {
    /// A host with a fresh bridge id, on a fresh broker, once it has subscribed to `Config/+`.
    pub async fn start(name: &str, fleet: Fleet) -> HostHarness {
        let broker = Arc::new(FakeBroker::start_multi().await);
        broker.cap_connections(80);
        HostHarness::join(broker, &crate::instance_name(name), fleet, |host| host).await
    }

    /// A host with bridge id `bridge` on `broker` — a second process of the kind, or a restart —
    /// after `tune` has set what the scenario needs, once it has subscribed to `Config/+`.
    pub async fn join(broker: Arc<FakeBroker>, bridge: &str, fleet: Fleet, tune: impl FnOnce(Host) -> Host) -> HostHarness {
        let harness = HostHarness::join_unwaited(broker, bridge, fleet, tune);
        let configs = format!("{ROOT}/Config/+");
        assert!(
            harness.broker.wait_until(bound(), |b| b.subscriptions().contains(&configs)).await,
            "the host never subscribed to {configs}"
        );
        harness
    }

    /// The same, without waiting for its subscriptions (a broker that refuses or holds them).
    pub fn join_unwaited(broker: Arc<FakeBroker>, bridge: &str, fleet: Fleet, tune: impl FnOnce(Host) -> Host) -> HostHarness {
        let log = Capture::start(bridge);
        let host = Host::new(KIND)
            .root(ROOT)
            .name(bridge)
            .broker(&broker.address())
            .timing(quick_timing())
            .mqtt_timing(quick_mqtt())
            .device_stable(Duration::from_millis(300))
            .factory_timeout(Duration::from_secs(2))
            .version("conformance");
        let running = tune(host).device(fleet.factory()).start().expect("the host starts");
        HostHarness { broker, bridge: bridge.to_owned(), fleet, log, running: Some(running) }
    }

    /// An instance's topics.
    pub fn topics(&self, instance: &str) -> Topics {
        Topics::new(ROOT, instance).expect("the instance's topics")
    }

    /// `Active/{bridge id}`.
    pub fn active_topic(&self) -> String {
        format!("{ROOT}/Active/{}", self.bridge)
    }

    /// `Config/{instance}`.
    pub fn config_topic(&self, instance: &str) -> String {
        format!("{ROOT}/Config/{instance}")
    }

    /// The Store's Config for `instance`, naming this process: `{"bridge", "via", …fields}`,
    /// retained.
    pub fn configure(&self, instance: &str, fields: Value) -> bool {
        let mut config = json!({ "bridge": KIND, "via": self.bridge });
        if let (Value::Object(config), Value::Object(fields)) = (&mut config, fields) {
            config.extend(fields);
        }
        self.configure_raw(instance, config.to_string().as_bytes())
    }

    /// A raw retained `Config/{instance}`.
    pub fn configure_raw(&self, instance: &str, payload: &[u8]) -> bool {
        self.broker.publish(&self.config_topic(instance), payload.to_vec(), &SendOptions::default().retained())
    }

    /// The Store's delete: an empty retained `Config/{instance}`.
    pub fn delete(&self, instance: &str) -> bool {
        self.configure_raw(instance, b"")
    }

    /// What was published on `topic`.
    pub fn published(&self, topic: &str) -> Vec<Received> {
        self.broker.received_on(topic)
    }

    /// The instance's latest `Status`, and the `via` it carried.
    pub fn status_via(&self, instance: &str) -> Option<(String, Option<String>)> {
        self.published(&self.topics(instance).status())
            .last()
            .map(|r| (String::from_utf8_lossy(&r.payload).into_owned(), r.user_property("via").map(str::to_owned)))
    }

    /// The instance's latest `Status`.
    pub fn status(&self, instance: &str) -> Option<String> {
        self.status_via(instance).map(|(status, _)| status)
    }

    /// Every `Status` the instance published, in order.
    pub fn statuses(&self, instance: &str) -> Vec<String> {
        self.published(&self.topics(instance).status()).iter().map(|r| String::from_utf8_lossy(&r.payload).into_owned()).collect()
    }

    /// A target's latest published `State`.
    pub fn state(&self, instance: &str, target: &str) -> Option<Value> {
        self.published(&self.topics(instance).state(target)).last().and_then(|r| serde_json::from_slice(&r.payload).ok())
    }

    /// Every `Error/{instance}`, as JSON.
    pub fn errors(&self, instance: &str) -> Vec<Value> {
        self.published(&self.topics(instance).error()).iter().filter_map(|r| serde_json::from_slice(&r.payload).ok()).collect()
    }

    /// The `Error/{instance}`s naming `reason`.
    pub fn errors_for(&self, instance: &str, reason: &str) -> Vec<Value> {
        self.errors(instance).into_iter().filter(|e| e["reason"] == reason).collect()
    }

    /// The Store's write of one property, retained.
    pub fn desire(&self, instance: &str, target: &str, property: &str, value: Value, req: Req) -> bool {
        self.broker.publish(&self.topics(instance).desired(target, property), value.to_string(), &req.options().retained())
    }

    /// A command to `instance`, with a response topic and correlation data when `reply` is given.
    pub fn command(&self, instance: &str, payload: Value, reply: Option<(&str, &[u8])>) -> bool {
        let mut options = SendOptions::default().expiry(30);
        if let Some((topic, correlation)) = reply {
            options = options.response_topic(topic).correlation_data(correlation.to_vec());
        }
        self.broker.publish(&self.topics(instance).command(), payload.to_string(), &options)
    }

    /// The replies on `topic`, as JSON.
    pub fn replies(&self, topic: &str) -> Vec<Value> {
        self.published(topic).iter().filter_map(|r| serde_json::from_slice(&r.payload).ok()).collect()
    }

    /// Whether `filter` is subscribed now.
    pub fn subscribed(&self, filter: &str) -> bool {
        self.broker.subscriptions().iter().any(|f| f == filter)
    }

    /// The broker's events, as labels, from the `n`-th connection on (1-based).
    pub fn labels(&self) -> Vec<String> {
        self.broker
            .events()
            .into_iter()
            .map(|event| match event {
                BrokerEvent::Connected => "connect".to_owned(),
                BrokerEvent::Subscribed(f) => format!("sub {f}"),
                BrokerEvent::Unsubscribed(f) => format!("unsub {f}"),
                BrokerEvent::Published(r) => format!("pub {} {}", r.topic, String::from_utf8_lossy(&r.payload)),
                BrokerEvent::Disconnected => "disconnect".to_owned(),
            })
            .collect()
    }

    /// Wait until `ready` holds, polling, up to `within`. Returns whether it held.
    pub async fn eventually(&self, within: Duration, mut ready: impl FnMut(&HostHarness) -> bool) -> bool {
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

    /// The latest device built for `instance`, once one is (within [`bound`]).
    pub async fn built(&self, instance: &str) -> Device {
        assert!(self.eventually(bound(), |h| h.fleet.device(instance).is_some()).await, "no device was built for {instance}");
        self.fleet.device(instance).expect("built")
    }

    /// The host's running handle.
    pub fn running(&mut self) -> &mut Running {
        self.running.as_mut().expect("the host is running")
    }

    /// The process dies without a word: its parts dropped unstopped, its connection closed without
    /// a DISCONNECT, so the broker publishes its last will.
    pub fn abort(&mut self) {
        drop(self.running.take());
    }

    /// Stop the host (bounded), keeping the harness to look at what it left. Whether it stopped in
    /// time.
    pub async fn stop_host(&mut self) -> bool {
        let running = self.running.take().expect("the host is running");
        tokio::time::timeout(bound(), running.stop()).await.unwrap_or(false)
    }

    /// Stop the host (bounded) and check the lanes. Whether it stopped in time.
    pub async fn stop(mut self) -> bool {
        let stopped = self.stop_host().await;
        self.check_lanes();
        stopped
    }

    /// Check every publish so far: QoS 1; each lane's schema and retain flag; and every `Status` and
    /// `State` of a configured instance carries exactly one `via` user property (multi-device spec
    /// §3, §4.5).
    pub fn check_lanes(&self) {
        for r in self.broker.received() {
            let topic = r.topic.as_str();
            let mut segments = topic.splitn(3, '/');
            let (Some(ROOT), Some(lane)) = (segments.next(), segments.next()) else {
                continue;
            };
            let (schema_lane, retained) = match lane {
                "Active" => ("active", true),
                "Status" => ("status", true),
                "Version" => ("version", true),
                "State" => ("state", true),
                "Error" => ("error", false),
                "Event" => {
                    let reply = serde_json::from_slice::<Value>(&r.payload).is_ok_and(|v| v["kind"] == "reply");
                    (if reply { "reply" } else { "event" }, false)
                }
                "Desired" | "Command" | "Config" => panic!("a host published on a lane the Store owns: {topic}"),
                _ => ("reply", false),
            };
            assert_eq!(r.qos, QoS::AtLeastOnce, "{topic} was not published at QoS 1");
            assert_eq!(r.retain, retained, "{topic}: retained is {}, its lane says {retained}", r.retain);
            if matches!(lane, "Status" | "State") {
                let vias = r.properties.as_ref().map_or(0, |p| p.user_properties.iter().filter(|(k, _)| k == "via").count());
                assert_eq!(vias, 1, "{topic} carried {vias} `via` user properties, not one");
            }
            if r.payload.is_empty() && matches!(lane, "State" | "Status") {
                continue;
            }
            let instance = schema::instance(schema_lane, &r.payload).unwrap_or_else(|e| panic!("{topic}: {e}"));
            if let Err(e) = schema::check(schema_lane, &instance) {
                panic!("{topic} does not conform to its lane's schema: {e}; the payload: {instance}");
            }
        }
    }
}

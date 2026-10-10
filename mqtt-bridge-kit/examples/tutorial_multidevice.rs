//! The tutorial's multi-device bridge (`docs/tutorial-writing-a-bridge.md`, section 12): one
//! process for many smart plugs, each its own network device and its own instance — a
//! *device-direct* bridge on the multi-device host (`Host`), where the LampNet bridge was
//! *controller-fronted* (`Bridge`, one process per controller).
//!
//! A plug speaks one line at a time over TCP: it greets with `PLUG 1`, answers `GET` with `on` or
//! `off`, and `SET on` / `SET off` with `OK` (or `ERR <message>`). Its one target is `socket`, with
//! one property, `on`.
//!
//! Run it: `cargo run --example tutorial_multidevice -- --broker <host>`; its tests:
//! `cargo test --example tutorial_multidevice`.

use std::io;
use std::process::ExitCode;
use std::time::Duration;

use mqtt_bridge_kit::{
    CommandCall, DeviceConfig, DeviceDriver, DriverInfo, FactoryError, Feedback, Host, LinkContext, LinkError, OpError, PropertyMap, Scope,
    TargetState, Timing, Value, Writes,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;

/// A line past this many bytes is the link failing, not a reply.
const MAX_LINE: u64 = 256;

/// A plug's address: `host[:port]`, port 4020 by default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    host: String,
    port: u16,
}

impl Address {
    /// `host` or `host:port`.
    pub fn parse(text: &str) -> Result<Address, String> {
        match text.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port = port.parse().map_err(|_| format!("`{text}`: `{port}` is not a port"))?;
                Ok(Address { host: host.to_owned(), port })
            }
            None if !text.is_empty() => Ok(Address { host: text.to_owned(), port: 4020 }),
            _ => Err(format!("`{text}` is not `host[:port]`")),
        }
    }
}

/// One smart plug: one instance, one device link.
pub struct Plug {
    address: Address,
    link: Option<(BufReader<OwnedReadHalf>, OwnedWriteHalf)>,
}

impl Plug {
    /// A driver for the plug at `address`; it connects when the runtime asks.
    pub fn new(address: Address) -> Plug {
        Plug { address, link: None }
    }

    /// Send one line and read the plug's one-line reply. `ERR` is the plug refusing.
    async fn exchange(&mut self, line: &str) -> Result<String, OpError> {
        let (reader, writer) = self.link.as_mut().ok_or_else(|| LinkError::new("the plug is not connected"))?;
        writer.write_all(format!("{line}\n").as_bytes()).await.map_err(LinkError::from)?;
        let reply = next_line(reader).await.map_err(LinkError::from)?.ok_or_else(|| LinkError::new("the plug closed the connection"))?;
        match reply.strip_prefix("ERR ") {
            Some(message) => Err(OpError::Rejected(message.to_owned())),
            None => Ok(reply),
        }
    }
}

/// The next line, without its line end; `None` at the end of the connection.
async fn next_line(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    if reader.take(MAX_LINE).read_until(b'\n', &mut line).await? == 0 {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a line past its limit, or cut off"));
    }
    Ok(Some(String::from_utf8_lossy(&line).trim_end_matches('\r').to_owned()))
}

/// The factory: a plug's driver from its instance's Config — the driver fields are `{"address":
/// "host[:port]"}`. A Config without a usable address is `Malformed`: the host says so on the
/// instance's `Error` lane and serves the instance as failed until its Config changes.
fn plug(config: &DeviceConfig) -> Result<Plug, FactoryError> {
    let address = config.fields().get("address").and_then(Value::as_str);
    let address = address.ok_or_else(|| FactoryError::Malformed("`address`, a string `host[:port]`, is required".into()))?;
    Address::parse(address).map(Plug::new).map_err(FactoryError::Malformed)
}

impl DeviceDriver for Plug {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            writes: Writes::Acknowledged,
            feedback: Feedback::Observed,
            commands: Vec::new(),
            timing: Timing {
                connect: Duration::from_secs(3),
                operation: Duration::from_secs(3),
                retry: Duration::from_secs(3),
                poll: Some(Duration::from_secs(10)),
                ..Timing::default()
            },
        }
    }

    async fn connect(&mut self, _ctx: &mut LinkContext) -> Result<(), LinkError> {
        let stream = TcpStream::connect((self.address.host.as_str(), self.address.port)).await?;
        let (read, writer) = stream.into_split();
        let mut reader = BufReader::new(read);
        match next_line(&mut reader).await? {
            Some(greeting) if greeting.starts_with("PLUG ") => {}
            _ => return Err(LinkError::new("the device did not greet like a plug")),
        }
        self.link = Some((reader, writer));
        Ok(())
    }

    async fn disconnect(&mut self) {
        self.link = None;
    }

    async fn read_state(&mut self, _scope: Scope) -> Result<Vec<TargetState>, OpError> {
        let on = match self.exchange("GET").await?.as_str() {
            "on" => true,
            "off" => false,
            other => return Err(OpError::Unusable(format!("not `on` or `off`: `{other}`"))),
        };
        let mut values = PropertyMap::new();
        values.insert("on".into(), json!(on));
        Ok(vec![TargetState::new("socket", values)])
    }

    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError> {
        if target != "socket" {
            return Err(OpError::Unsupported(format!("a plug has no target `{target}`")));
        }
        match values.get("on") {
            Some(Value::Bool(on)) => self.exchange(if *on { "SET on" } else { "SET off" }).await.map(drop),
            _ => Err(OpError::Unsupported("a plug's socket has one property, `on`, a boolean".into())),
        }
    }

    async fn execute(&mut self, _target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError> {
        Err(OpError::Unsupported(format!("no command `{}`", call.command)))
    }

    async fn configure(&mut self, _target: &str, _config: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

fn main() -> ExitCode {
    Host::new("plug").root("Plugs").app_name("plugs").version("0.1.0").from_cli().device(plug).run()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use mqtt_bridge_kit::topics::Topics;
    use mqtt_bridge_kit::{MqttTiming, Running, TimingOverrides};
    use mqtt_test_broker::{FakeBroker, SendOptions};
    use tokio::net::TcpListener;

    use super::*;

    /// The bound on any one wait.
    const BOUND: Duration = Duration::from_secs(10);

    /// A stand-in plug: the line protocol, its socket in memory.
    struct StandIn {
        port: u16,
        on: Arc<Mutex<bool>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for StandIn {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl StandIn {
        async fn start() -> StandIn {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind the stand-in");
            let port = listener.local_addr().expect("its address").port();
            let on = Arc::new(Mutex::new(false));
            let state = on.clone();
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let state = state.clone();
                    tokio::spawn(async move {
                        let (read, mut write) = stream.into_split();
                        let mut lines = BufReader::new(read).lines();
                        let _ = write.write_all(b"PLUG 1\n").await;
                        while let Ok(Some(line)) = lines.next_line().await {
                            let reply = match line.as_str() {
                                "GET" => if *state.lock().unwrap() { "on" } else { "off" }.to_owned(),
                                "SET on" | "SET off" => {
                                    *state.lock().unwrap() = line == "SET on";
                                    "OK".to_owned()
                                }
                                other => format!("ERR no such command `{other}`"),
                            };
                            if write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            StandIn { port, on, task }
        }

        fn address(&self) -> String {
            format!("127.0.0.1:{}", self.port)
        }

        fn on(&self) -> bool {
            *self.on.lock().unwrap()
        }
    }

    /// A host of plugs, `plugs-test`, against `broker`.
    fn host(broker: &FakeBroker) -> Running {
        let timing = TimingOverrides { retry: Some(Duration::from_millis(300)), poll: Some(None), ..TimingOverrides::default() };
        let mqtt = MqttTiming { poll_retry: Duration::from_millis(100), ..MqttTiming::default() };
        Host::new("plug")
            .root("Plugs")
            .name("plugs-test")
            .broker(&broker.address())
            .timing(timing)
            .mqtt_timing(mqtt)
            .device(plug)
            .start()
            .expect("the host starts")
    }

    /// The Store's Config of plug `instance`: kind `plug`, served by `plugs-test`.
    fn configure(broker: &FakeBroker, instance: &str, fields: Value) {
        let mut config = json!({"bridge": "plug", "via": "plugs-test"});
        config.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        broker.publish(&format!("Plugs/Config/{instance}"), config.to_string(), &SendOptions::default().retained());
    }

    /// An instance's latest `Status` and the `via` that published it.
    fn status(broker: &FakeBroker, instance: &str) -> Option<(String, Option<String>)> {
        let topics = Topics::new("Plugs", instance).unwrap();
        broker
            .received_on(&topics.status())
            .last()
            .map(|r| (String::from_utf8_lossy(&r.payload).into_owned(), r.user_property("via").map(str::to_owned)))
    }

    fn by_us(status: &str) -> Option<(String, Option<String>)> {
        Some((status.to_owned(), Some("plugs-test".to_owned())))
    }

    /// Two plugs in one process: each its own instance, its own `Status`, tagged with the process's
    /// bridge id. The one that is out of reach is said `unreachable` and retried; the other serves.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_plugs_in_one_process() {
        let broker = FakeBroker::start().await;
        let kitchen = StandIn::start().await;
        let running = host(&broker);
        assert!(broker.wait_for_subscription("Plugs/Config/+", BOUND).await, "the host never subscribed to the Configs");
        configure(&broker, "kitchen", json!({"address": kitchen.address()}));
        configure(&broker, "porch", json!({"address": "127.0.0.1:1"}));
        assert!(broker.wait_until(BOUND, |b| status(b, "kitchen") == by_us("connected")).await, "{:?}", status(&broker, "kitchen"));
        assert!(broker.wait_until(BOUND, |b| status(b, "porch") == by_us("unreachable")).await, "{:?}", status(&broker, "porch"));
        let desired = Topics::new("Plugs", "kitchen").unwrap().desired("socket", "on");
        broker.publish(&desired, "true", &SendOptions::default().retained());
        assert!(broker.wait_until(BOUND, |_| kitchen.on()).await, "the write never reached the kitchen plug");
        assert!(running.stop().await);
    }

    /// A Config the factory cannot use: one `Error` on the instance's lane, naming why, and the
    /// instance failed (`unreachable`) until its Config changes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_config_without_an_address_is_malformed() {
        let broker = FakeBroker::start().await;
        let running = host(&broker);
        assert!(broker.wait_for_subscription("Plugs/Config/+", BOUND).await);
        configure(&broker, "hall", json!({}));
        let errors = Topics::new("Plugs", "hall").unwrap().error();
        assert!(broker.wait_until(BOUND, |b| !b.received_on(&errors).is_empty()).await, "no Error said why");
        let error: Value = serde_json::from_slice(&broker.received_on(&errors)[0].payload).unwrap();
        assert_eq!(error["reason"], "malformed", "{error}");
        assert!(broker.wait_until(BOUND, |b| status(b, "hall") == by_us("unreachable")).await, "{:?}", status(&broker, "hall"));
        assert!(running.stop().await);
    }

    /// Every Rust block of the tutorial's section 12 is a verbatim excerpt of this file.
    #[test]
    fn the_tutorials_multidevice_code_is_this_files() {
        let tutorial = include_str!("../docs/tutorial-writing-a-bridge.md");
        let section = &tutorial[tutorial.find(MULTIDEVICE_SECTION).expect("the tutorial's multi-device section")..];
        let section = section.find("\n## mqtt_ac v2").map_or(section, |end| &section[..end]);
        let source = include_str!("tutorial_multidevice.rs");
        let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let source = squash(source);
        let mut blocks = 0;
        let mut rest = section;
        while let Some(start) = rest.find("```rust\n") {
            let after = &rest[start + "```rust\n".len()..];
            let end = after.find("```").expect("a closed block");
            assert!(
                source.contains(&squash(&after[..end])),
                "a block of section 12 is not in examples/tutorial_multidevice.rs:\n{}",
                &after[..end]
            );
            blocks += 1;
            rest = &after[end..];
        }
        assert!(blocks >= 4, "section 12 quotes only {blocks} blocks");
    }

    /// The heading the tutorial's multi-device section starts at.
    const MULTIDEVICE_SECTION: &str = "## 12. Many devices in one process";
}

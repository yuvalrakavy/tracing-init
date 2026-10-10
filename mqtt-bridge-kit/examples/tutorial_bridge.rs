//! The tutorial's bridge (`docs/tutorial-writing-a-bridge.md`): a driver for an imaginary lamp
//! controller, "LampNet", on the Bridge Runtime. The tutorial quotes this file; a test below checks
//! that every Rust block of the tutorial is verbatim from it, so the text cannot drift from code
//! that compiles.
//!
//! LampNet is a line protocol over TCP. The controller greets with `LAMPNET 1`, then answers one
//! command line at a time with zero or more body lines and a final `OK` or `ERR <message>`:
//!
//! ```text
//! GET [<lamp>]                    ->  lamp <id> <on|off> <level 0-100> ... OK
//! SET <lamp> on <on|off>          ->  OK
//! SET <lamp> level <0-100>        ->  OK    (or ERR level must be 0-100)
//! FLASH <lamp>                    ->  OK
//! ```
//!
//! and, at any time, tells of a button pressed at a lamp: `EVT lamp <id> <on|off> <level>`.
//!
//! Run it against a controller with
//! `cargo run --example tutorial_bridge -- --instance home --broker <host> --controller <host>`.

use std::io;
use std::process::ExitCode;
use std::time::Duration;

use mqtt_bridge_kit::{
    Bridge, CommandCall, CommandClass, CommandSpec, DeviceDriver, DriverInfo, FailureClass, Feedback, LinkContext, LinkError, LinkHandle,
    OpError, PropertyFailure, PropertyMap, Scope, TargetState, Timing, Value, Writes,
};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

/// The controller's address, `host` or `host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Address {
    host: String,
    port: u16,
}

impl Address {
    /// The port LampNet controllers listen on unless told otherwise.
    const DEFAULT_PORT: u16 = 4010;

    fn parse(text: &str) -> Result<Address, String> {
        match text.rsplit_once(':') {
            Some((host, port)) if !host.is_empty() => {
                let port = port.parse().map_err(|_| format!("`{text}`: `{port}` is not a port"))?;
                Ok(Address { host: host.to_owned(), port })
            }
            Some(_) => Err(format!("`{text}` names no host")),
            None if text.is_empty() => Err("the controller's address names no host".into()),
            None => Ok(Address { host: text.to_owned(), port: Address::DEFAULT_PORT }),
        }
    }
}

/// The longest line the controller may send. A line past it has lost its framing: the link is
/// dropped rather than buffered on until the deadline.
const MAX_LINE: u64 = 4096;

/// What the driver declares about itself: acknowledged writes (`OK` is final), observed feedback,
/// two commands, and timing that suits a controller on the local network.
fn info() -> DriverInfo {
    DriverInfo {
        writes: Writes::Acknowledged,
        feedback: Feedback::Observed,
        commands: vec![CommandSpec::new("flash", CommandClass::Momentary), CommandSpec::new("count", CommandClass::WithReply)],
        timing: Timing {
            connect: Duration::from_secs(5),
            operation: Duration::from_secs(5),
            retry: Duration::from_secs(3),
            // The controller tells of button presses itself; the poll is the safety net.
            poll: Some(Duration::from_secs(30)),
            ..Timing::default()
        },
    }
}

/// One open connection: where commands go, and where the helper hands their replies.
struct Link {
    writer: OwnedWriteHalf,
    replies: mpsc::UnboundedReceiver<String>,
}

/// The LampNet driver.
pub struct LampNet {
    address: Address,
    link: Option<Link>,
    /// The current link's handle, for reporting from an operation.
    handle: Option<LinkHandle>,
}

impl LampNet {
    /// A driver for the controller at `address`; it connects when the runtime asks.
    pub fn new(address: Address) -> LampNet {
        LampNet { address, link: None, handle: None }
    }

    /// Send one command line and collect its reply: the body lines up to `OK`, or the controller's
    /// `ERR` as a refusal. No timeout here: the runtime bounds the whole operation.
    async fn exchange(&mut self, line: &str) -> Result<Vec<String>, OpError> {
        let link = self.link.as_mut().ok_or_else(|| LinkError::new("the controller is not connected"))?;
        link.writer.write_all(format!("{line}\n").as_bytes()).await.map_err(LinkError::from)?;
        let mut body = Vec::new();
        loop {
            let Some(reply) = link.replies.recv().await else {
                return Err(LinkError::new("the controller closed the connection").into());
            };
            if reply == "OK" {
                return Ok(body);
            }
            match reply.strip_prefix("ERR ") {
                Some(message) => return Err(OpError::Rejected(message.to_owned())),
                None => body.push(reply),
            }
        }
    }

    /// Read a lamp again and report it, so its `State` follows a write at once.
    async fn read_back(&mut self, lamp: &str) -> Result<(), LinkError> {
        match self.read_state(Scope::Target(lamp.to_owned())).await {
            Ok(states) => {
                if let Some(handle) = &self.handle {
                    for state in states {
                        handle.report().state(&state.target, state.values, false);
                    }
                }
                Ok(())
            }
            Err(OpError::Link(e)) => Err(e),
            // A lamp that cannot be read now is read again by the next poll.
            Err(_) => Ok(()),
        }
    }
}

impl DeviceDriver for LampNet {
    fn info(&self) -> DriverInfo {
        info()
    }

    /// A TCP connection, up to the controller's greeting; then the helper that reads the rest.
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
        let Address { host, port } = &self.address;
        let stream = TcpStream::connect((host.as_str(), *port)).await?;
        let _ = stream.set_nodelay(true);
        let (read, writer) = stream.into_split();
        let mut reader = BufReader::new(read);
        match next_line(&mut reader).await? {
            Some(greeting) if greeting.starts_with("LAMPNET ") => {}
            _ => return Err(LinkError::new("the controller did not greet like LampNet")),
        }
        let (hand_over, replies) = mpsc::unbounded_channel();
        ctx.spawn(read_lines(reader, hand_over, ctx.handle()));
        self.link = Some(Link { writer, replies });
        self.handle = Some(ctx.handle());
        Ok(())
    }

    async fn disconnect(&mut self) {
        self.link = None;
        self.handle = None;
    }

    /// `GET`: every lamp, or one.
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError> {
        let line = match &scope {
            Scope::Target(lamp) => {
                check_lamp(lamp)?;
                format!("GET {lamp}")
            }
            Scope::All | Scope::Any => "GET".to_owned(),
        };
        self.exchange(&line).await?.iter().map(|line| parse_lamp(line)).collect()
    }

    /// A `SET` per property. A property the controller refuses, or that is not a lamp's, costs only
    /// itself: the rest are still sent, and the outcome names the failed ones. Properties go in
    /// key order, which is the order this controller wants: `level`, then `on`.
    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError> {
        check_lamp(target)?;
        let mut failed = Vec::new();
        let mut sent = false;
        for (property, value) in values {
            let done = match set_line(target, property, value) {
                Ok(line) => {
                    sent = true;
                    self.exchange(&line).await.map(drop)
                }
                Err(refused) => Err(refused),
            };
            match done {
                Ok(()) => {}
                Err(OpError::Link(e)) => return Err(OpError::Link(e)),
                Err(other) => failed.push(failure(property, other)),
            }
        }
        if sent {
            self.read_back(target).await?;
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(OpError::Partial(failed))
        }
    }

    /// `flash` blinks a lamp; `count` answers how many lamps the controller has.
    async fn execute(&mut self, target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError> {
        match call.command.as_str() {
            "flash" => {
                let lamp = target.ok_or_else(|| OpError::Rejected("`flash` needs a target".into()))?;
                check_lamp(lamp)?;
                self.exchange(&format!("FLASH {lamp}")).await?;
                Ok(None)
            }
            "count" => {
                let lamps = self.exchange("GET").await?;
                Ok(Some(json!({ "count": lamps.len() })))
            }
            other => Err(OpError::Unsupported(format!("no command `{other}`"))),
        }
    }

    /// A lamp needs no setup.
    async fn configure(&mut self, _target: &str, _config: Option<&Value>) -> Result<(), OpError> {
        Ok(())
    }
}

/// The helper that owns the connection's read half for the life of the link. A line that tells of a
/// button press is state, reported at once; every other line is a reply, handed to the operation
/// waiting for it. The end of the connection is the link lost.
async fn read_lines(mut reader: BufReader<OwnedReadHalf>, replies: mpsc::UnboundedSender<String>, handle: LinkHandle) {
    loop {
        match next_line(&mut reader).await {
            Ok(Some(line)) => match line.strip_prefix("EVT ") {
                Some(event) => {
                    if let Ok(state) = parse_lamp(event) {
                        handle.report().state(&state.target, state.values, false);
                    }
                }
                None => {
                    if replies.send(line).is_err() {
                        return;
                    }
                }
            },
            Ok(None) => return handle.link_lost("the controller closed the connection"),
            Err(e) => return handle.link_lost(format!("the controller's line could not be read: {e}")),
        }
    }
}

/// The next line, without its line end; `None` at the end of the connection.
async fn next_line(reader: &mut BufReader<OwnedReadHalf>) -> io::Result<Option<String>> {
    let mut line = Vec::new();
    let read = reader.take(MAX_LINE).read_until(b'\n', &mut line).await?;
    if read == 0 {
        return Ok(None);
    }
    if line.pop() != Some(b'\n') {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "a line past its limit, or cut off"));
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    Ok(Some(String::from_utf8_lossy(&line).into_owned()))
}

/// A lamp is a word of the line protocol: a target that is not cannot be sent.
fn check_lamp(lamp: &str) -> Result<(), OpError> {
    if lamp.chars().any(char::is_whitespace) {
        return Err(OpError::Rejected(format!("`{lamp}` is not a lamp name")));
    }
    Ok(())
}

/// `lamp <id> <on|off> <level>` as a target's state. A line that is anything else is `Unusable`:
/// the controller answered, the driver cannot read it, and the link is fine.
fn parse_lamp(text: &str) -> Result<TargetState, OpError> {
    let unusable = || OpError::Unusable(format!("not a lamp line: `{text}`"));
    let words: Vec<&str> = text.split_whitespace().collect();
    let &["lamp", id, power, level] = words.as_slice() else {
        return Err(unusable());
    };
    let on = match power {
        "on" => true,
        "off" => false,
        _ => return Err(unusable()),
    };
    let level: u8 = level.parse().map_err(|_| unusable())?;
    let mut values = PropertyMap::new();
    values.insert("on".to_owned(), json!(on));
    values.insert("level".to_owned(), json!(level));
    Ok(TargetState::new(id, values))
}

/// The `SET` line for one property, or why the driver will not send it.
fn set_line(lamp: &str, property: &str, value: &Value) -> Result<String, OpError> {
    match (property, value) {
        ("on", Value::Bool(on)) => Ok(format!("SET {lamp} on {}", if *on { "on" } else { "off" })),
        ("level", Value::Number(level)) => Ok(format!("SET {lamp} level {level}")),
        ("on" | "level", _) => Err(OpError::Rejected(format!("`{property}` was given {value}, which is the wrong type"))),
        _ => Err(OpError::Unsupported(format!("a lamp has no property `{property}`"))),
    }
}

/// One property's failure, with its class.
fn failure(property: &str, error: OpError) -> PropertyFailure {
    let (class, error) = match error {
        OpError::Unusable(m) => (FailureClass::Unusable, m),
        OpError::Unsupported(m) => (FailureClass::Unsupported, m),
        OpError::Rejected(m) => (FailureClass::Rejected, m),
        other => (FailureClass::Rejected, other.to_string()),
    };
    PropertyFailure { property: property.to_owned(), class, error }
}

fn main() -> ExitCode {
    let bridge = Bridge::new("Lamps")
        .app_name("lampnet")
        .version("lampnet 0.1.0")
        .option("controller", "the lamp controller, host[:port] (port 4010 by default)")
        .require("controller")
        .from_cli();
    let address = bridge.value("controller").map(Address::parse);
    let bridge = match &address {
        Some(Err(problem)) => bridge.usage_error(problem.as_str()),
        _ => bridge,
    };
    if let Some(code) = bridge.usage_exit() {
        return code;
    }
    match address {
        Some(Ok(address)) => bridge.run(LampNet::new(address)),
        // Not reached: without the option, `require` made it a usage error, said above.
        _ => ExitCode::from(2),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use mqtt_bridge_kit::topics::Topics;
    use mqtt_bridge_kit::{MqttTiming, Running, TimingOverrides};
    use mqtt_test_broker::{FakeBroker, SendOptions};
    use tokio::net::TcpListener;
    use tokio::sync::broadcast;

    use super::*;

    const WITHIN: Duration = Duration::from_secs(5);
    /// How long a negative control waits for something that must not happen.
    const BRIEFLY: Duration = Duration::from_secs(2);

    // ---- The stand-in device: LampNet, as far as the tests need it. ----

    /// What a test tells every connection to do.
    #[derive(Clone)]
    enum Push {
        Line(String),
        Close,
    }

    /// A LampNet controller on 127.0.0.1 that keeps its lamps in memory.
    struct StandIn {
        port: u16,
        /// Each lamp: on, level.
        lamps: Arc<Mutex<BTreeMap<String, (bool, u8)>>>,
        flashed: Arc<Mutex<Vec<String>>>,
        accepted: Arc<AtomicUsize>,
        pushes: broadcast::Sender<Push>,
    }

    impl StandIn {
        async fn start(lamps: &[(&str, bool, u8)]) -> StandIn {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
            let device = StandIn {
                port: listener.local_addr().expect("its address").port(),
                lamps: Arc::new(Mutex::new(lamps.iter().map(|(id, on, level)| ((*id).to_owned(), (*on, *level))).collect())),
                flashed: Arc::default(),
                accepted: Arc::default(),
                pushes: broadcast::channel(16).0,
            };
            let (lamps, flashed, accepted, pushes) =
                (device.lamps.clone(), device.flashed.clone(), device.accepted.clone(), device.pushes.clone());
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    accepted.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(serve(stream, lamps.clone(), flashed.clone(), pushes.subscribe()));
                }
            });
            device
        }

        fn address(&self) -> Address {
            Address { host: "127.0.0.1".to_owned(), port: self.port }
        }

        fn lamp(&self, id: &str) -> Option<(bool, u8)> {
            self.lamps.lock().unwrap().get(id).copied()
        }

        fn accepted(&self) -> usize {
            self.accepted.load(Ordering::SeqCst)
        }

        /// Someone presses the button of a lamp.
        fn press(&self, id: &str, on: bool, level: u8) {
            self.lamps.lock().unwrap().insert(id.to_owned(), (on, level));
            let line = format!("EVT lamp {id} {} {level}", if on { "on" } else { "off" });
            let _ = self.pushes.send(Push::Line(line));
        }

        /// The controller drops every connection.
        fn close_connections(&self) {
            let _ = self.pushes.send(Push::Close);
        }
    }

    /// One connection of the stand-in: the greeting, then commands and pushes until either ends.
    async fn serve(
        stream: TcpStream,
        lamps: Arc<Mutex<BTreeMap<String, (bool, u8)>>>,
        flashed: Arc<Mutex<Vec<String>>>,
        mut pushes: broadcast::Receiver<Push>,
    ) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        if write.write_all(b"LAMPNET 1\n").await.is_err() {
            return;
        }
        loop {
            let reply = tokio::select! {
                line = lines.next_line() => match line {
                    Ok(Some(line)) => answer(&line, &lamps, &flashed),
                    _ => return,
                },
                push = pushes.recv() => match push {
                    Ok(Push::Line(line)) => format!("{line}\n"),
                    _ => return,
                },
            };
            if write.write_all(reply.as_bytes()).await.is_err() {
                return;
            }
        }
    }

    /// The controller's answer to one command line.
    fn answer(line: &str, lamps: &Mutex<BTreeMap<String, (bool, u8)>>, flashed: &Mutex<Vec<String>>) -> String {
        let words: Vec<&str> = line.split_whitespace().collect();
        let mut lamps = lamps.lock().unwrap();
        let lamp_line = |id: &str, (on, level): (bool, u8)| format!("lamp {id} {} {level}\n", if on { "on" } else { "off" });
        match words.as_slice() {
            ["GET"] => lamps.iter().map(|(id, lamp)| lamp_line(id, *lamp)).chain(["OK\n".to_owned()]).collect(),
            ["GET", id] => match lamps.get(*id) {
                Some(lamp) => lamp_line(id, *lamp) + "OK\n",
                None => "ERR no such lamp\n".to_owned(),
            },
            ["SET", id, "on", power @ ("on" | "off")] => match lamps.get_mut(*id) {
                Some(lamp) => {
                    lamp.0 = *power == "on";
                    "OK\n".to_owned()
                }
                None => "ERR no such lamp\n".to_owned(),
            },
            ["SET", id, "level", level] => match (lamps.get_mut(*id), level.parse::<u8>()) {
                (Some(lamp), Ok(level)) if level <= 100 => {
                    lamp.1 = level;
                    "OK\n".to_owned()
                }
                (Some(_), _) => "ERR level must be 0-100\n".to_owned(),
                (None, _) => "ERR no such lamp\n".to_owned(),
            },
            ["FLASH", id] if lamps.contains_key(*id) => {
                flashed.lock().unwrap().push((*id).to_owned());
                "OK\n".to_owned()
            }
            _ => "ERR what?\n".to_owned(),
        }
    }

    // ---- The rig: a FakeBroker, the stand-in, and the bridge between them, in-process. ----

    struct Rig {
        broker: FakeBroker,
        device: StandIn,
        topics: Topics,
        running: Running,
    }

    impl Rig {
        /// Start a bridge around the driver `make` builds for the stand-in's address, and wait
        /// until it reports the device connected. `None` if it never does.
        async fn start<D: DeviceDriver>(make: impl FnOnce(Address) -> D) -> Option<Rig> {
            let broker = FakeBroker::start().await;
            let device = StandIn::start(&[("hall", false, 0), ("porch", true, 80)]).await;
            let topics = Topics::new("Lamps", "test").expect("topics");
            let timing = TimingOverrides {
                connect: Some(Duration::from_secs(2)),
                operation: Some(Duration::from_secs(2)),
                retry: Some(Duration::from_millis(300)),
                poll: Some(None),
                ..TimingOverrides::default()
            };
            let mqtt = MqttTiming { poll_retry: Duration::from_millis(100), ..MqttTiming::default() };
            let running = Bridge::new("Lamps")
                .instance("test")
                .broker(&broker.address())
                .timing(timing)
                .mqtt_timing(mqtt)
                .start(make(device.address()))
                .expect("the bridge starts");
            let rig = Rig { broker, device, topics, running };
            rig.wait(|rig| rig.status().as_deref() == Some("connected")).await.then_some(rig)
        }

        async fn wait(&self, ready: impl FnMut(&Rig) -> bool) -> bool {
            let mut ready = ready;
            let deadline = tokio::time::Instant::now() + WITHIN;
            while tokio::time::Instant::now() < deadline {
                if ready(self) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        }

        /// The latest `Status` the bridge published.
        fn status(&self) -> Option<String> {
            let last = self.broker.received_on(&self.topics.status()).pop()?;
            Some(String::from_utf8_lossy(&last.payload).into_owned())
        }

        /// A lamp's latest published `State`.
        fn state(&self, lamp: &str) -> Option<Value> {
            let last = self.broker.received_on(&self.topics.state(lamp)).pop()?;
            serde_json::from_slice(&last.payload).ok()
        }

        /// The `Error`s the bridge published for `reason`.
        fn errors(&self, reason: &str) -> Vec<Value> {
            let all = self.broker.received_on(&self.topics.error());
            all.iter().filter_map(|e| serde_json::from_slice::<Value>(&e.payload).ok()).filter(|e| e["reason"] == reason).collect()
        }

        /// What the Store does to write one property: a retained `Desired`.
        fn desire(&self, lamp: &str, property: &str, value: Value) {
            let options = SendOptions::default().retained().expiry(60).user_property("conflict", "RequestWins");
            self.broker.publish(&self.topics.desired(lamp, property), value.to_string(), &options);
        }

        /// What the Store does to write several properties at once: the target message, then each
        /// property's retained copy.
        fn desire_many(&self, lamp: &str, values: Value) {
            let options = SendOptions::default().expiry(60).user_property("conflict", "RequestWins");
            self.broker.publish(&self.topics.desired_target(lamp), values.to_string(), &options);
            for (property, value) in values.as_object().expect("an object") {
                self.desire(lamp, property, value.clone());
            }
        }

        async fn stop(self) -> bool {
            self.running.stop().await
        }
    }

    // ---- The scenarios. Each is a function from a driver to a verdict, so a broken driver can be
    // ---- run through it too: a test that cannot fail proves nothing (the negative controls).

    fn ensure(holds: bool, what: impl FnOnce() -> String) -> Result<(), String> {
        if holds {
            Ok(())
        } else {
            Err(what())
        }
    }

    /// The device is read at connect, and a write reaches it and comes back as `State`.
    async fn a_write_reaches_the_lamp<D: DeviceDriver>(make: impl FnOnce(Address) -> D) -> Result<(), String> {
        let rig = Rig::start(make).await.ok_or("the bridge never reported the device connected")?;
        ensure(rig.state("hall") == Some(json!({ "on": false, "level": 0 })), || format!("hall's first State: {:?}", rig.state("hall")))?;
        rig.desire("hall", "level", json!(40));
        let reached = rig.wait(|rig| rig.device.lamp("hall") == Some((false, 40))).await;
        ensure(reached, || format!("the lamp is {:?}", rig.device.lamp("hall")))?;
        let reported = rig.wait(|rig| rig.state("hall").is_some_and(|s| s["level"] == 40)).await;
        ensure(reported, || format!("hall's State after the write: {:?}", rig.state("hall")))?;
        ensure(rig.stop().await, || "the bridge did not stop in time".to_owned())
    }

    /// One property of a write is refused: `Error` names it alone, the other is applied, and the
    /// link stays up.
    async fn a_refused_property_costs_only_itself<D: DeviceDriver>(make: impl FnOnce(Address) -> D) -> Result<(), String> {
        let rig = Rig::start(make).await.ok_or("the bridge never reported the device connected")?;
        rig.desire_many("hall", json!({ "on": true, "level": 500 }));
        let reported = rig.wait(|rig| !rig.errors("rejected").is_empty()).await;
        ensure(reported, || "the refused level got no Error".to_owned())?;
        let errors = rig.errors("rejected");
        ensure(errors.len() == 1, || format!("{} Errors, not one: {errors:?}", errors.len()))?;
        ensure((errors[0]["target"].as_str(), errors[0]["property"].as_str()) == (Some("hall"), Some("level")), || {
            format!("the Error names the wrong thing: {}", errors[0])
        })?;
        ensure(rig.device.lamp("hall") == Some((true, 0)), || format!("the lamp is {:?}: `on` was not applied", rig.device.lamp("hall")))?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        ensure(rig.device.accepted() == 1, || "the refusal was taken for a lost link".to_owned())?;
        ensure(rig.status().as_deref() == Some("connected"), || format!("the status is {:?}", rig.status()))?;
        ensure(rig.stop().await, || "the bridge did not stop in time".to_owned())
    }

    // ---- The tests. ----

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_is_applied_and_its_state_comes_back() {
        assert_eq!(a_write_reaches_the_lamp(LampNet::new).await, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_partly_refused_write_is_a_partial() {
        assert_eq!(a_refused_property_costs_only_itself(LampNet::new).await, Ok(()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_button_press_at_the_lamp_becomes_state() {
        let rig = Rig::start(LampNet::new).await.expect("connected");
        rig.device.press("porch", false, 0);
        assert!(rig.wait(|rig| rig.state("porch") == Some(json!({ "on": false, "level": 0 }))).await, "{:?}", rig.state("porch"));
        assert!(rig.stop().await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_connection_is_made_again() {
        let rig = Rig::start(LampNet::new).await.expect("connected");
        rig.device.close_connections();
        assert!(rig.wait(|rig| rig.device.accepted() >= 2 && rig.status().as_deref() == Some("connected")).await, "not reconnected");
        let statuses: Vec<String> =
            rig.broker.received_on(&rig.topics.status()).iter().map(|s| String::from_utf8_lossy(&s.payload).into_owned()).collect();
        assert!(statuses.iter().any(|s| s == "unreachable"), "the outage was never said: {statuses:?}");
        assert!(rig.stop().await);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn commands_act_once_and_a_reply_follows_its_correlation() {
        let rig = Rig::start(LampNet::new).await.expect("connected");
        let command = |payload: Value, correlation: &'static str| {
            let options = SendOptions::default().expiry(30).response_topic("Lamps/replies").correlation_data(correlation);
            rig.broker.publish(&rig.topics.command(), payload.to_string(), &options)
        };
        assert!(command(json!({ "target": "hall", "command": "flash" }), "one"));
        assert!(command(json!({ "command": "count" }), "two"));
        assert!(rig.wait(|rig| rig.broker.received_on("Lamps/replies").len() == 2).await, "two replies expected");
        assert_eq!(*rig.device.flashed.lock().unwrap(), ["hall"], "flash must act once");
        let replies = rig.broker.received_on("Lamps/replies");
        let count = replies.iter().find(|r| r.correlation_data().is_some_and(|c| &c[..] == b"two")).expect("the count's reply");
        let count: Value = serde_json::from_slice(&count.payload).expect("JSON");
        assert_eq!((count["kind"].as_str(), count["value"]["count"].as_u64()), (Some("reply"), Some(2)));
        assert!(rig.stop().await);
    }

    // ---- Negative controls: the same scenarios against drivers broken on purpose must fail. ----

    /// The real driver, broken on purpose in `apply`: `writes` says whether it sends anything at
    /// all, and `tamper` changes the result it returns.
    struct Tamper {
        inner: LampNet,
        writes: bool,
        tamper: fn(Result<(), OpError>) -> Result<(), OpError>,
    }

    impl DeviceDriver for Tamper {
        fn info(&self) -> DriverInfo {
            self.inner.info()
        }
        async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError> {
            self.inner.connect(ctx).await
        }
        async fn disconnect(&mut self) {
            self.inner.disconnect().await
        }
        async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError> {
            self.inner.read_state(scope).await
        }
        async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError> {
            let result = if self.writes { self.inner.apply(target, values).await } else { Ok(()) };
            (self.tamper)(result)
        }
        async fn execute(&mut self, target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError> {
            self.inner.execute(target, call).await
        }
        async fn configure(&mut self, target: &str, config: Option<&Value>) -> Result<(), OpError> {
            self.inner.configure(target, config).await
        }
    }

    fn tampered(writes: bool, tamper: fn(Result<(), OpError>) -> Result<(), OpError>) -> impl FnOnce(Address) -> Tamper {
        move |address| Tamper { inner: LampNet::new(address), writes, tamper }
    }

    /// The scenario must have failed, and for the reason the control breaks.
    fn assert_caught(verdict: Result<Result<(), String>, tokio::time::error::Elapsed>, reason: &str) {
        match verdict {
            Ok(Err(why)) => assert!(why.contains(reason), "caught, but for another reason: {why}"),
            other => panic!("the scenario passed a driver broken on purpose: {other:?}"),
        }
    }

    /// A driver that swallows the controller's refusal: the Store would never hear of it.
    #[tokio::test(flavor = "multi_thread")]
    async fn control_a_swallowed_refusal_is_caught() {
        let verdict = tokio::time::timeout(BRIEFLY * 4, a_refused_property_costs_only_itself(tampered(true, |_| Ok(())))).await;
        assert_caught(verdict, "the refused level got no Error");
    }

    /// A driver that reports a refusal as a lost link: the whole outage machinery runs for it.
    #[tokio::test(flavor = "multi_thread")]
    async fn control_a_refusal_taken_for_a_lost_link_is_caught() {
        let misclassify = |result| match result {
            Err(OpError::Partial(failures)) => Err(OpError::Link(LinkError::new(format!("{failures:?}")))),
            other => other,
        };
        let verdict = tokio::time::timeout(BRIEFLY * 4, a_refused_property_costs_only_itself(tampered(true, misclassify))).await;
        assert_caught(verdict, "the refused level got no Error");
    }

    /// A driver that says `Ok` and sends nothing: the lamp does not move.
    #[tokio::test(flavor = "multi_thread")]
    async fn control_a_write_that_is_never_sent_is_caught() {
        let verdict = tokio::time::timeout(BRIEFLY * 4, a_write_reaches_the_lamp(tampered(false, |result| result))).await;
        assert_caught(verdict, "the lamp is Some((false, 0))");
    }

    /// Every Rust block of the tutorial is quoted from this file, verbatim — but section 12's, the
    /// multi-device bridge, which `examples/tutorial_multidevice.rs` checks against itself.
    #[test]
    fn the_tutorials_code_is_this_files() {
        let whole = include_str!("../docs/tutorial-writing-a-bridge.md");
        let section = whole.find("## 12. Many devices in one process").expect("the tutorial's multi-device section");
        let after = whole[section..].find("\n## mqtt_ac v2").map_or(whole.len(), |end| section + end);
        let tutorial = format!("{}{}", &whole[..section], &whole[after..]);
        let tutorial = tutorial.as_str();
        let source = include_str!("tutorial_bridge.rs");
        let squash = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
        let source = squash(source);
        let mut blocks = 0;
        let mut rest = tutorial;
        while let Some(start) = rest.find("```rust\n") {
            let after = &rest[start + "```rust\n".len()..];
            let end = after.find("```").expect("a closed block");
            let block = squash(&after[..end]);
            assert!(source.contains(&block), "a tutorial block is not in examples/tutorial_bridge.rs:\n{}", &after[..end]);
            blocks += 1;
            rest = &after[end..];
        }
        assert!(blocks >= 8, "the tutorial quotes only {blocks} blocks");
    }
}

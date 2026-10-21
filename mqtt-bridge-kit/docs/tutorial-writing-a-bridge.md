# Tutorial: writing a bridge on the Bridge Runtime

This tutorial writes a complete bridge, step by step, for an imaginary device: **LampNet**, a lamp
controller that speaks a line protocol over TCP. By the end you have a driver, a `main`, tests that
run against an in-process broker and a stand-in controller (and that you have proved can fail), and a
systemd unit.

Every Rust block below is quoted from one file, [`examples/tutorial_bridge.rs`](../examples/tutorial_bridge.rs),
which builds with `cargo build --examples` and whose tests run with `cargo test --example tutorial_bridge`.
A test in that file checks that each block of this page is a verbatim excerpt of it, so the text
cannot drift from code that compiles.

Section 12 then turns to the other shape: a *device-direct* bridge, many devices in one process,
on the multi-device host (`Host`); its code is a second file,
[`examples/tutorial_multidevice.rs`](../examples/tutorial_multidevice.rs), checked against that
section the same way.

The worked real-world example is **mqtt_ac v2**, the CoolMaster air-conditioning bridge, which was the
runtime's first user. Where it does something the tutorial does not, the text says so and links to its
files (see [mqtt_ac v2, file by file](#mqtt_ac-v2-file-by-file) at the end).

Reference material: the crate's [README](../README.md) (what the runtime owns, the command line, the
guarantees), the rustdoc (`cargo doc --no-deps --open`) and the
[runtime spec](../../docs/superpowers/specs/2026-10-04-bridge-runtime-design.md), which is the source
of truth.

## 0. What you write, and what you do not

A bridge is one process that connects one device to MQTT, in the Store's *Bridge Kit* grammar. The
runtime (`mqtt-bridge-kit`) owns everything that is *policy*: the topics, the MQTT connection and its
outages, the queue of requests while the device is down, the conflict rule, reports, replies, logging,
signals and the bounded shutdown. You write what only you can know: **how to talk to the device.** That
is one trait, `DeviceDriver`, with eight operations (`probe` has a default), each with a meaning, a
deadline and an error class.

The runtime calls your driver from one task at a time, so a protocol that needs strict
command-then-reply sequencing gets it for free. It bounds every call with a deadline, so **your driver
needs no timeouts of its own**: an overrun is a lost link, never a hang.

## 1. The device

LampNet, as the driver sees it. The controller greets with `LAMPNET 1`, then answers one command line
at a time with zero or more body lines and a final `OK` or `ERR <message>`:

```text
GET [<lamp>]                    ->  lamp <id> <on|off> <level 0-100> ...  OK
SET <lamp> on <on|off>          ->  OK
SET <lamp> level <0-100>        ->  OK    (or ERR level must be 0-100)
FLASH <lamp>                    ->  OK
```

and, at any time, when somebody presses a button at a lamp, it sends `EVT lamp <id> <on|off> <level>`.

That last line is why the driver needs a **helper**: a reply and an unsolicited line arrive on the same
TCP stream, so something must always be reading it, and only that something can tell them apart.

The Store sees each lamp as a **target** (`hall`, `porch`) with two **properties**, `on` (a boolean) and
`level` (0 to 100). A target is one topic segment: `{Root}/State/{instance}/hall` carries
`{"on":true,"level":40}`. The grammar and the lanes are the Store's; the README's
[topic grammar](../README.md#the-topic-grammar-and-lanes) section has the table.

## 2. The project

```toml
[package]
name = "lampnet"
version = "0.1.0"
edition = "2021"

[dependencies]
# One revision of the runtime for the bridge and its tests: pin it. mqtt_ac v2 does.
mqtt-bridge-kit = { git = "https://github.com/yuvalrakavy/tracing-init.git", rev = "<a commit of that repository>" }
serde_json = "1"
tokio = { version = "1", features = ["io-util", "net", "sync"] }

[dev-dependencies]
mqtt-test-broker = { git = "https://github.com/yuvalrakavy/tracing-init.git", rev = "<the same commit>" }
tokio = { version = "1", features = ["io-util", "macros", "net", "rt", "rt-multi-thread", "sync", "time"] }
```

There is no MQTT client in the list, no signal handling and no logging crate to configure: the runtime
owns them. The bridge's `Cargo.toml` must **not** enable the kit's `test-seams` feature; it is for the
runtime's own conformance suite and refuses to compile in any other repository.

## 3. `info()`: what the driver declares

`info()` is called once, at start, and must be pure (no I/O). It tells the runtime four things:

```rust
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
```

- **`writes`.** `Acknowledged` means a successful `apply` is final: the device confirmed it. LampNet
  answers `OK`, so that is right. A device that only *accepts* a write and can prove it later (a
  receiver that echoes state, a gateway with no write acknowledgement) declares `Unacknowledged`: the
  runtime then keeps each written value *unproven* until a later `probe` or read succeeds, and writes
  it again if the link dies first. A driver that declares `Unacknowledged` must be able to `probe`.
- **`feedback`.** `Observed`: the device's state is read back. `Assumed` is for one-way devices (IR,
  DMX): the runtime then publishes each applied value as `State` marked `"assumed": true`. A target's
  `Config` can override it.
- **`commands`.** The momentary commands `execute` takes, each with a class: `Momentary` (queued in
  order while the device is up, *refused* while it is down, because by the time it is back a "flash"
  means nothing), `Read` (coalesced, dropped while down) or `WithReply` (like `Momentary`, with an
  answer for the command's response topic). `refresh` and `assume` are standard and the runtime's:
  do not list them.
- **`timing`.** Defaults for the connect and operation bounds, the pacing of reconnects, the outage
  WARN threshold, the poll interval and the probe timing. Each is overridable on the command line, so
  an operator can tune a slow controller without a rebuild. Choose bounds from the device's worst
  case, not its typical one: they decide when a slow answer becomes a lost link.

## 4. The driver, and its one open connection

```rust
/// One open connection: where commands go, and where the helper hands their replies.
struct Link {
    writer: OwnedWriteHalf,
    replies: mpsc::UnboundedReceiver<String>,
}
```

```rust
/// The LampNet driver.
pub struct LampNet {
    address: Address,
    link: Option<Link>,
    /// The current link's handle, for reporting from an operation.
    handle: Option<LinkHandle>,
}
```

The driver is moved to a thread of its own with a single-threaded runtime, so it is `Send + 'static`
but its futures need not be `Send`. It keeps the **write half** of the connection and the **receiving
end of a channel**; the **read half** belongs to the helper (step 5). It also keeps the current link's
`LinkHandle`, which is how an operation reports state later: the `LinkContext` is handed only to
`connect`.

```rust
/// A driver for the controller at `address`; it connects when the runtime asks.
pub fn new(address: Address) -> LampNet {
    LampNet { address, link: None, handle: None }
}
```

The shape of a request is `exchange`: write one line, then take replies off the channel until the
final `OK` or `ERR`.

```rust
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
```

Three things to notice, because they are the heart of the error model:

- A failed write, or the channel closing (the helper saw the connection end), is a **`LinkError`**: the
  connection failed. It converts into `OpError::Link` with `?` or `.into()`. The runtime drops the
  link, holds state requests, refuses momentary commands, and reconnects at the paced rate.
- The controller's `ERR` is **`OpError::Rejected`**: the device is fine, it refused *this* request. The
  runtime publishes an `Error` naming the target and property, and the link stays up.
- There is no `timeout` here. The runtime bounds the operation and drops this future at the deadline.

## 5. `connect` and the helper reader

```rust
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
```

`connect` opens the TCP connection, checks the greeting and starts the helper with `ctx.spawn`. It is
bounded by `Timing::connect`; a failure is a `LinkError`, and the runtime retries at most once per
`Timing::retry`. Connecting is not yet *proof of life*: the runtime next calls `read_state`, and only a
good answer makes the link `Up`.

The helper owns the read half for the life of the link:

```rust
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
```

`ctx.spawn` has three properties that make this safe to write:

- The helper is **cancelled when the link drops**. A reconnect starts a fresh one; nothing is left
  reading a dead socket.
- It is **watched**. A helper that returns (or panics) is the link lost, so a reader that gives up
  cannot leave the bridge "connected" and deaf. Here the helper says *why* with `handle.link_lost(...)`
  first, which becomes the outage's logged reason. A helper that is meant to end (a delayed re-query, a
  debounce timer) uses `ctx.spawn_transient` instead.
- It runs on the worker's own single-threaded runtime, so it need not be `Send`.

A button press is **state**, reported at once through `handle.report().state(..)`. A reporter call
returns immediately and never waits on MQTT, so a helper can call it from anywhere without risk.

The line reader is bounded. A device that never sends a line end would otherwise be buffered until the
deadline; here a line past its limit is an I/O error, which the helper turns into a lost link:

```rust
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
```

`disconnect` is best effort and quick; it is called after every link that ended (a failed `connect`
included) and at shutdown, so it must cope with a half-opened link:

```rust
async fn disconnect(&mut self) {
    self.link = None;
    self.handle = None;
}
```

> **A synchronous library?** If the device library blocks (a serial port, a vendor SDK), do not call it
> from an `async fn` directly: a blocked thread cannot be interrupted, and the watchdog will declare the
> worker dead and restart the process. Use `ctx.blocking(|| ...)` (or `handle.blocking`) instead. It
> runs the call on a thread of its own, held to the operation's deadline; an overrun abandons the
> thread and counts as a lost link. The number of abandoned threads is budgeted (four for operations,
> sixteen for helpers), so give the library its own timeout where it has one. mqtt_ac needs none of this
> (it speaks TCP through tokio).

## 6. `read_state`

```rust
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
```

`read_state` is called after connecting (as proof of life), on the poll interval, for the Store's
`refresh` command and before the runtime resolves an outage conflict. It must read the device, never a
cache. Whatever it returns is published as `State`; a target it returns is *merged* into the retained
model, and a target it does not return is not withdrawn: to say a target is gone, call
`report.remove(target)`.

`Scope::All` and `Scope::Any` (the cheapest read; the default `probe` uses it) are the same here.

A line that cannot be parsed is the controller *answering something the driver cannot read*:

```rust
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
```

That is **`OpError::Unusable`**, and it is not a reason to reconnect: the runtime publishes an `Error`
once and passes the request over. (Reconnecting for an unreadable answer was a hot loop in the bridges
this runtime replaced.) Reserve `Link` for the connection failing.

## 7. `apply`, and `Partial`

A request can arrive for several properties at once (a target-level `Desired`), and the driver should
apply them together. Each property can fail on its own: the controller can refuse a value, or the
Store can ask for a property a lamp does not have. `apply` reports exactly which, with
`OpError::Partial`:

```rust
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
```

The contract of `Partial`: **the properties it names failed, and every other value was applied.** The
runtime publishes an `Error` for each named property alone, and the link stays up. So the loop keeps
going after a refused property (turning a lamp off is not lost to a refused level), and stops only for
a `Link` error, where the runtime will hold the whole request for the next connection.

A plain `Rejected`, `Unusable` or `Unsupported` from `apply` means the *whole* write failed. Return
`Partial` only when some values were applied.

Each property's failure carries its class, so the Store learns whether a value was refused or does not
exist:

```rust
/// The `SET` line for one property, or why the driver will not send it.
fn set_line(lamp: &str, property: &str, value: &Value) -> Result<String, OpError> {
    match (property, value) {
        ("on", Value::Bool(on)) => Ok(format!("SET {lamp} on {}", if *on { "on" } else { "off" })),
        ("level", Value::Number(level)) => Ok(format!("SET {lamp} level {level}")),
        ("on" | "level", _) => Err(OpError::Rejected(format!("`{property}` was given {value}, which is the wrong type"))),
        _ => Err(OpError::Unsupported(format!("a lamp has no property `{property}`"))),
    }
}
```

```rust
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
```

After the writes, `apply` reads the lamp back and reports it, so its `State` follows at once instead of
at the next poll (`read_back`, in step 4's `impl`). The runtime does not publish a written value as
state for an `Acknowledged` device: the device may have clamped it, and only a read says what it holds.

## 8. `execute` and `configure`

```rust
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
```

`execute` acts **once** and never retries: the runtime does not retry a momentary command, and a retry
here could act twice. `target` is `None` for a command that names none. The `Ok(Some(value))` of a
`WithReply` command goes to the command's MQTT v5 response topic, carrying its correlation data; any
failure also reaches the Store as an `Error` naming the command.

An unknown command cannot reach the driver (the runtime answers `unsupported` for a name `info()` did
not list), but the `match` keeps the driver honest about its own list.

`configure` is called when a target's `Config` arrives or is retracted (`None`), only while the link is
up. LampNet has no per-lamp setup:

```rust
/// A lamp needs no setup.
async fn configure(&mut self, _target: &str, _config: Option<&Value>) -> Result<(), OpError> {
    Ok(())
}
```

## 9. `main`

```rust
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
```

`Bridge::new("Lamps")` names the family, which is the default MQTT root (`--root` overrides it). The
chain declares the driver's own option, `--controller`, and makes it required. After `from_cli()`:

- `bridge.value("controller")` is the raw text; the driver's own parse of it can fail, and
  `usage_error` reports that as a usage error: said on stderr with the usage text, exit status 2,
  before any driver is built or anything started.
- `usage_exit()` is `--help` or a usage error said; `main` returns its code.
- `run(driver)` is the rest of the process. It installs the panic hook, takes SIGINT and SIGTERM
  *first*, starts the logging, starts the bridge, and blocks until a stop signal (the bounded
  shutdown, then success) or until one of the bridge's parts dies (a failure exit, for the service
  manager to restart). Return its `ExitCode` from `main`; never call `process::exit` after it.

Try it:

```sh
cargo run --example tutorial_bridge -- --help
cargo run --example tutorial_bridge -- --instance home --broker mqtt.local --controller 192.168.1.40
```

`--instance` and `--broker` are always required. Everything else in `--help` (the timing overrides,
`--log-config`, the MQTT timings) is the runtime's.

## 10. Tests

The point of the structure is that a bridge's tests need no real broker and no real device, and that
they run the *real* runtime around the driver. Three pieces:

1. **A stand-in device**: a small TCP server that speaks LampNet over the same line protocol, keeps its
   lamps in memory, and lets the test press a button or drop the connection. It is not a mock of the
   driver; it is the device, as far as the tests need it. (It lives in the example's `tests` module,
   about 130 lines, and is worth reading.)
2. **`FakeBroker`** (the [`mqtt-test-broker`](../../mqtt-test-broker) crate): an MQTT v5 broker on
   `127.0.0.1`, in your test process. It plays the Store's side: your test publishes a `Desired` or a
   `Command` as the Store would (`publish`, with `SendOptions` for retain, expiry, user properties,
   response topic and correlation data) and reads what the bridge published (`received_on`). Never
   point a test at a real broker.
3. **`Bridge::start(driver)`**: runs the bridge *in-process*, on the test's tokio runtime (no signals,
   no logging start, no panic hook), and returns a `Running` whose `stop()` is the bounded shutdown.

The rig starts the three together:

```rust
struct Rig {
    broker: FakeBroker,
    device: StandIn,
    topics: Topics,
    running: Running,
}
```

```rust
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
```

The timings are short, so an outage plays out in a second: `retry` is 300 ms and polling is off. Note
that the test never sleeps for a guess: it waits for the `Status` to say `connected` and polls for the
condition it wants, with a bound, so a regression fails the test instead of hanging it.

A scenario is a function from a *driver* to a verdict. The first one: a write reaches the lamp, and its
state comes back.

```rust
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
```

The second one is the `Partial` contract: one property of a write is refused by the device (level 500
is past its limit), and the scenario checks everything the contract promises: one `Error`, naming the
target and the *refused* property alone; the other property applied; the link not dropped (the
controller saw exactly one connection); the status still `connected`.

```rust
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
```

The tests that run them, and two more that cover what only a live link shows: a button press at the lamp
(the helper reader) and a lost connection that the runtime reconnects:

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_write_is_applied_and_its_state_comes_back() {
    assert_eq!(a_write_reaches_the_lamp(LampNet::new).await, Ok(()));
}
```

```rust
#[tokio::test(flavor = "multi_thread")]
async fn a_button_press_at_the_lamp_becomes_state() {
    let rig = Rig::start(LampNet::new).await.expect("connected");
    rig.device.press("porch", false, 0);
    assert!(rig.wait(|rig| rig.state("porch") == Some(json!({ "on": false, "level": 0 }))).await, "{:?}", rig.state("porch"));
    assert!(rig.stop().await);
}
```

```rust
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
```

### Negative controls

A test that cannot fail proves nothing. Passing is only half of a check: **break the code on purpose
and see the test go red, for the right reason.** That is a *negative control*, and the scenarios above
were written as functions of a driver precisely so that a broken driver can be run through them.
`Tamper` wraps the real driver and spoils `apply` in one chosen way:

```rust
/// The real driver, broken on purpose in `apply`: `writes` says whether it sends anything at
/// all, and `tamper` changes the result it returns.
struct Tamper {
    inner: LampNet,
    writes: bool,
    tamper: fn(Result<(), OpError>) -> Result<(), OpError>,
}
```

```rust
fn tampered(writes: bool, tamper: fn(Result<(), OpError>) -> Result<(), OpError>) -> impl FnOnce(Address) -> Tamper {
    move |address| Tamper { inner: LampNet::new(address), writes, tamper }
}
```

and a control asserts that the scenario *fails*, and fails with the message of the guard it removes:

```rust
/// The scenario must have failed, and for the reason the control breaks.
fn assert_caught(verdict: Result<Result<(), String>, tokio::time::error::Elapsed>, reason: &str) {
    match verdict {
        Ok(Err(why)) => assert!(why.contains(reason), "caught, but for another reason: {why}"),
        other => panic!("the scenario passed a driver broken on purpose: {other:?}"),
    }
}
```

```rust
/// A driver that swallows the controller's refusal: the Store would never hear of it.
#[tokio::test(flavor = "multi_thread")]
async fn control_a_swallowed_refusal_is_caught() {
    let verdict = tokio::time::timeout(BRIEFLY * 4, a_refused_property_costs_only_itself(tampered(true, |_| Ok(())))).await;
    assert_caught(verdict, "the refused level got no Error");
}
```

```rust
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
```

```rust
/// A driver that says `Ok` and sends nothing: the lamp does not move.
#[tokio::test(flavor = "multi_thread")]
async fn control_a_write_that_is_never_sent_is_caught() {
    let verdict = tokio::time::timeout(BRIEFLY * 4, a_write_reaches_the_lamp(tampered(false, |result| result))).await;
    assert_caught(verdict, "the lamp is Some((false, 0))");
}
```

Three drivers, each wrong in a way real drivers go wrong: one that swallows the device's refusal (the
Store would wait for a change that never comes), one that reports a refusal as a lost link (the whole
outage machinery runs for a request the device merely declined), and one that says `Ok` and sends
nothing. Each makes a scenario fail with the text that names what is missing; if a control ever passes,
the scenario had stopped checking that thing.

The habit generalises: for every guard you add to a driver, write the test, then undo the guard and
confirm the test fails. The runtime's own suite does this for every guard it has: see
[`docs/controls/2026-10-04-bridge-runtime-controls.toml`](../../docs/controls/2026-10-04-bridge-runtime-controls.toml),
and mqtt_ac's [`docs/v2-driver-controls.toml`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/docs/v2-driver-controls.toml),
one row per guard, each run to a red test.

Run everything:

```sh
cargo test --example tutorial_bridge
```

What the stand-in does *not* exercise is the runtime itself (outage episodes, the conflict rule,
shutdown ordering, the mailbox). That is the runtime's conformance suite
([`mqtt-bridge-conformance`](../../mqtt-bridge-conformance)), which runs against a scriptable fake device,
so your tests can stay about *your device*. See the README's [testing kit](../README.md#the-testing-kit).

## 11. Deploy

The bridge is one binary and a unit file. The runtime exits with `ExitCode::FAILURE` when a part of it
dies (a driver panic, a blocked device thread) and expects the service manager to restart it; that is
the recovery for the failures that cannot be handled in-process.

```ini
[Unit]
Description=LampNet bridge (the lamp controller on the Bridge Runtime)
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/opt/lampnet/lampnet --instance home --broker mqtt.local --controller 192.168.1.40 --log-config /opt/lampnet/logging.toml
WorkingDirectory=/opt/lampnet
# A failure exit (a dead part) and a crash both restart the bridge; a stop (SIGTERM) does not.
Restart=always
RestartSec=2
# The bounded shutdown takes at most about five seconds, and the runtime's thread grace one more.
TimeoutStopSec=15
User=lampnet

[Install]
WantedBy=multi-user.target
```

```sh
sudo cp lampnet.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now lampnet
journalctl -u lampnet -f
```

Notes, each of which is the runtime's behaviour rather than something the unit has to do:

- **Stopping.** `systemctl stop` sends SIGTERM. The bridge publishes `Active=false`, answers the commands
  still waiting, sends DISCONNECT and disconnects the driver, all within a few seconds, whatever the
  device or the broker is doing. `TimeoutStopSec` only has to be longer than that.
- **Starting before the network.** The bridge need not wait for the broker or the device: it retries
  each at a paced rate and publishes `Status` as it goes. `After=network-online.target` just avoids a
  burst of failed first attempts. The retained model is republished after every reconnect.
- **Logging.** The runtime starts [tracing-init](../../README.md) for you: a log file under `logs/` in
  the working directory (so `WorkingDirectory` matters, and the user needs to be able to write there)
  and GELF, plus whatever the `--log-config` file adds (a `logging.toml`; see
  `examples/full_toml.logging.toml` at the repository root). The destinations are lossy: a log stream
  that stalls cannot stall the bridge, and a logging that cannot start does not stop it. The log is
  structured; the fleet's `kind` names (`device_unreachable`, `device_recovered`, `command_rejected`,
  `worker_died`) are what to search for.
- **No stdout.** Nothing the bridge needs goes to stdout or stderr; the unit may send both to the
  journal or to files, and a full pipe nobody drains cannot hold the bridge.
- **Two bridges, one MQTT client id.** The client id is derived from the root and instance; do not run
  two processes with the same pair against one broker.
- **Versioning.** `Bridge::version("lampnet 0.1.0")` is the retained `Version` the Store shows.

## 12. Many devices in one process

LampNet is *controller-fronted*: one controller, its lamps behind it as targets, one process per
controller. That is the `Bridge` you have built. Some devices are each their own network endpoint —
an AV receiver per room (mqtt_ynca), an IR emitter per room, a smart plug per socket — and running a
process per device does not scale. Those bridges are *device-direct*: **one process serves many
devices**, each an instance of its own, over one MQTT session. That is the runtime's multi-device
host, `Host` (multi-device spec, `docs/superpowers/specs/2026-10-10-multi-device-host-design.md`).

| | `Bridge` (controller-fronted) | `Host` (device-direct) |
|---|---|---|
| devices per process | one, fixed: `--instance` | as many as the Store configures |
| where a device comes from | the driver you pass to `run` | a factory, from each instance's `Config/{instance}` |
| the process's name | the instance | the bridge id, `--name` (default: the kind) |
| `Status`, `State` | untagged | tagged with the user property `via` = the bridge id |
| a blocked driver | ends the process | ends the process (all its devices restart with it) |

These are not two runtimes. The `Bridge` you built is this host's **N = 1 mode**: a `Host` with one
fixed instance, named from `--instance`, served by the driver you pass to `run` instead of one a
factory builds, and owned by the process itself — so it subscribes no `Config/+`, waits for no
barrier, tags nothing with `via`, and says `Active/{instance}`. Its session, its CONNACK duties (R1's,
in R1's order), its publication, its packet-id budget and its shutdown flush are the host's own code.
Moving from one shape to the other changes where drivers come from, not the machinery around them.

This section's code is a second example file, [`examples/tutorial_multidevice.rs`](../examples/tutorial_multidevice.rs):
a bridge for imaginary smart plugs that speak a one-line protocol over TCP (`GET` → `on`/`off`,
`SET on|off` → `OK`), one plug per instance, one target (`socket`) with one property (`on`). The
driver is written exactly as LampNet's was:

```rust
/// One smart plug: one instance, one device link.
pub struct Plug {
    address: Address,
    link: Option<(BufReader<OwnedReadHalf>, OwnedWriteHalf)>,
}
```

What is new is where a driver comes from. The Store creates each instance and publishes, retained,
`{Root}/Config/{instance}`:

```text
Plugs/Config/kitchen   {"bridge": "plug", "via": "plugs-lan1", "address": "10.0.0.31"}
```

`bridge` is the implementation kind (the host verifies it), `via` the process that serves the
instance (omitted: the kind's default bridge id, the kind itself), `"disabled": true` keeps the
instance owned but not served, and every other field is your driver's. An empty payload deletes the
instance. Your part is a **factory**: from an instance's driver fields to a driver.

```rust
fn plug(config: &DeviceConfig) -> Result<Plug, FactoryError> {
    let address = config.fields().get("address").and_then(Value::as_str);
    let address = address.ok_or_else(|| FactoryError::Malformed("`address`, a string `host[:port]`, is required".into()))?;
    Address::parse(address).map(Plug::new).map_err(FactoryError::Malformed)
}
```

- The factory checks the fields and builds the driver; it must not connect (that is `connect`, which
  the runtime calls, paced and bounded, once the device starts). It runs on a thread of its own,
  bounded by `--factory-timeout` (10 s), and the driver's `info()` after it under the same bound.
- `FactoryError::Malformed` (bad fields) and `FactoryError::Unusable` (valid, but no driver now)
  become one `Error/{instance}` naming the reason. The instance's last accepted Config stays in
  force; with none, the instance is *failed*: `Status=unreachable`, its commands refused, retried
  at the reconnect pacing and at once on any change of its Config.

`main` names the kind and gives the factory:

```rust
fn main() -> ExitCode {
    Host::new("plug").root("Plugs").app_name("plugs").version("0.1.0").from_cli().device(plug).run()
}
```

The command line is a `Bridge`'s with `--name <id>` (this process's bridge id, unique on the root;
several processes of one kind may share a root, one per subnet) and `--factory-timeout <d>` in place
of `--instance`. The retained `Version` says `<kind> <version>` (`plug 0.1.0`), which the Store's
Admin UI reads to offer this process for an instance's `Via`.

What the runtime does with each instance whose Config names the process (spec §4):

- **Its own subscriptions, in three steps** — its read-back (`State/{i}/+`, `Status/{i}`), its target
  Configs (`Config/{i}/+`), its requests — each SUBACK matched before the next, so the device starts
  with its previous `State` frozen as the conflict rule's "before" values, its target Configs
  applied first, then its retained requests through the conflict rule. The device starts once both
  the driver and the requests' grant are in.
- **Each device is R1's worker, mailbox and reports**: its own `Status`, outages, proof, catch-up,
  conflict rule and budgets. One device that never answers is that device's outage; the others are
  unaffected. A driver that *blocks its thread*, a blocking budget that stays full, or a factory that
  does not return ends the process (the supervisor restarts it, every device with it).
- **A Config change** builds the new driver while the old device keeps serving; only on success is
  the old one stopped (its waiting requests answered, its subscriptions removed) and the new one
  bootstrapped. **Disable** stops the device and keeps `Status=disabled`; **delete** stops it and
  clears the retained topics this process published.
- **The slot heals.** Every `Status` and `State` carries `via`; another process's late write to an
  owned instance's slot is overwritten by the owner.
- `Active/{bridge id}` is `true` once every subscription is granted and every owned instance's
  `Status` is published; its last will says `false`, so a process's death makes all its instances
  unavailable at once.

Tests look like LampNet's — a stand-in device, `FakeBroker`, the real runtime in-process — with
`Host::start` and the Store's Config published retained:

```rust
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
```

```rust
    fn configure(broker: &FakeBroker, instance: &str, fields: Value) {
        let mut config = json!({"bridge": "plug", "via": "plugs-test"});
        config.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        broker.publish(&format!("Plugs/Config/{instance}"), config.to_string(), &SendOptions::default().retained());
    }
```

```rust
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
```

Two plugs, one process: the porch plug (nothing listens at its address) is `unreachable` and
retried, while the kitchen plug serves; each `Status` is tagged `via = plugs-test`. Run them with
`cargo test --example tutorial_multidevice`. (Two processes on one root need a broker for several
clients at once: `FakeBroker::start_multi`.)

## mqtt_ac v2, file by file

[mqtt_ac](https://github.com/yuvalrakavy/mqtt_ac) v2 (landed at `main` 265df22) is the CoolMaster bridge
on this runtime; it is the tutorial's bridge with a real device's complications. The tutorial's pieces
and where mqtt_ac has them:

| Tutorial | mqtt_ac v2 |
|---|---|
| `main` with `Bridge::new(..).option(..).require(..).from_cli()`, a driver-option parse that can be a `usage_error`, `usage_exit`, `run` | [`src/main.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/main.rs): the same shape, plus `app_name`, `version`, and passing the command line's operation bound to the driver |
| `info()` | [`info()` in `src/driver.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/driver.rs#L38): `Acknowledged`, `Observed`, one `Momentary` command (`ResetFilter`), poll every 4 s |
| `connect` | [`connect`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/driver.rs#L471): TCP up to the CoolMaster's first `>` prompt; a prompt past a byte limit is a `LinkError` |
| the protocol, parsed and built apart from I/O | [`src/protocol.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/protocol.rs): `parse_listing` (a bad line costs only its own unit), `plan` / `plan_scaled` (the commands for a write, in the order the CoolMaster needs), `parse_reply` (the classes) |
| `read_state` | [`read_state`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/driver.rs#L506): an `ls2` listing; reports a unit that lost its power (`"powered": false`, kept in the model) and an Event when it returns, and retracts ghosts with `report.remove` |
| `apply` with `Partial` | [`apply`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/driver.rs#L558): a command per property, a refused one passed over, `OpError::Partial` for the failed ones; reads the unit back; reads a unit's scale before a setpoint, never guessing |
| `execute` | [`execute`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/driver.rs#L597): `ResetFilter`, then the unit read back |
| `ctx.handle()` for later reports | `Coolmaster::handle`: the read-back's `link_lost` for a link found dead after its command was confirmed |
| the start-up read-back | [`Coolmaster::seed`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/src/driver.rs#L290): `ctx.read_back()`, kept as a handle, seeds which units were last seen powered; see [`tests/restart.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/tests/restart.rs) |
| the helper reader | not needed: the CoolMaster speaks only when spoken to, so `exchange` reads its own reply. LampNet's unsolicited button presses are what make the helper necessary; a bridge whose device pushes (Lutron, YNCA) has one |
| tests: stand-in + `FakeBroker` + the real bridge | [`tests/support/`](https://github.com/yuvalrakavy/mqtt_ac/tree/265df22/tests/support) (a stand-in CoolMaster, a harness, a log recorder), [`tests/driver.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/tests/driver.rs) (the scenarios), [`tests/process.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/tests/process.rs) (the real binary: its command line and exit status) |
| negative controls | [`docs/v2-driver-controls.toml`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/docs/v2-driver-controls.toml): one row per guard, each run to a red test |
| deploy | [`ac.service`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/ac.service) and [`install_on_pi.sh`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/install_on_pi.sh) |
| the wait registry | [`docs/wait-registry.md`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/docs/wait-registry.md) and `tests/wait_registry.rs`: the driver's own awaits are tagged and registered like the runtime's |

Reading order if you are writing your first bridge: `main.rs` (40 lines), then `protocol.rs` for the
shape of a protocol module that does no I/O, then `driver.rs`' `impl DeviceDriver`, then one scenario in
`tests/driver.rs` end to end.

## Optional: a configuration server with a process lifetime

LampNet's reader belongs to its device link. A configuration web server or a
shared bus owner has a longer lifetime: it must keep running during MQTT/device
outages. Use `ProcessService` for that resource owner, and let its `Driver` be a
proxy using bounded requests to the service. `Bridge::run_managed(service)` owns
the signals, tracing-init, watchdog and bounded stop just as `run(driver)` does.

Construct the service without opening resources. Open them in `run`, call
`context.ready(proxy)?` once after preparation, then serve until
`context.cancelled().await`. `ready` validates the proxy's pure `info()` on the
service thread and transfers it without waiting for a broker or a device read.
Close service admission, cancel helpers and release resources before returning.
The proxy's `disconnect` must tolerate a closing service and must not stop the
process resource owner during a device reconnect.

Service futures and their local tasks run on an isolated current-thread runtime.
Bring helper failures back to `run`; do not silently detach fatal helper errors.
Keep I/O owned by that future/runtime so cancellation can destroy it. A blocked
helper or destructor cannot hold the process past its parent deadline, but it
does make shutdown unsuccessful. See the README's process-services section for
startup/stop bounds and the embedding counterparts `start_managed[_with]`.

## Optional: mutable physical inventories

LampNet's fixed target names need no additional methods. A controller that lets
the operator rename devices, reuse an address or change group membership needs a
mapping fence: a retained request from the previous occupant must not control its
replacement. Use the existing fixed `Bridge`, including `run_managed` when a
process service owns the controller; dynamic factory fencing is unsupported.

Return `RequestPolicy::Fenced(TargetGenerations { instance, targets })` from
`request_policy()`. Capture the complete initial table from the durable inventory,
including metadata targets. Give every changed mapping a fresh token that is
never reused for that target. The runtime calls this pure method once alongside
`info()`. Clients obtain their tokens from your complete inventory publication
and return them in the MQTT v5 user property `target-generation` (one value,
1–128 bytes in `0x21..0x7e`). Reads can omit it. Missing or retired tokens cannot
mutate a fenced mapping, and the runtime refuses its standard `assume` command.

Override `apply_with_context`, `execute_with_context` and
`configure_with_context` to pass the original `RequestContext` to your service.
Have `read_state_with_context` return `ContextualState` records stamped when the
physical read was issued. Use `Reporter::state_with_context`,
`event_with_context`, `remove_with_context` and `remove_property_with_context`
for unsolicited reports. A late read or callback keeps its original token;
absent/retired context cannot update the current model. Errors and replies also
retain the request's token outside their unchanged JSON payload.

Keep routing readiness separate from persistence. For a mapping edit:

1. Freeze service mutation admission and serialize with hardware operations.
2. Save the proposed inventory durably; remain frozen on uncertainty.
3. Call `reporter.replace_generations(&old_table, new_table)` and require success.
4. Report the complete new inventory with its instance token.
5. Expose the new service mapping as ready.

The table comparison rejects a stale owner. The runtime retires old queued,
in-flight and unproven work and its conflict history, preserving original tokens
on any retirement replies. A call already executing cannot be undone. Recheck
the context at the service's actual address-resolution/dispatch boundary too.
The pure `validate_request` hook lets the proxy reject transition-phase work
before cached Config success or an already-satisfied conflict decision; it does
no I/O and cannot override the runtime's own fence.

Test through a local fake broker: stale Desired/Config/commands and retractions,
rename away and back, address reuse, group-semantic changes, a delayed read across
replacement, and output sealed before replacement. Verify actual driver calls
and MQTT properties. Also retain the ordinary unfenced conformance suite so the
optional feature cannot alter existing bridges. The full runtime contract is
[`bridge-target-generations`](../../docs/superpowers/specs/2026-10-21-bridge-target-generations.md).

## A checklist before you ship a driver

- `info()` declares what is true: `writes`, `feedback`, every command and its class. A driver with
  `Unacknowledged` writes has a working `probe`.
- Only a failure of the *connection* is a `Link` error. A refusal is `Rejected`; an unparseable answer
  is `Unusable`; a name the device does not have is `Unsupported`. Per-property failures of an `apply`
  are one `Partial`.
- No timeouts of your own, and no blocking call on the async thread (`ctx.blocking` for a synchronous
  library). Every helper is started with `ctx.spawn` or `ctx.spawn_transient`, never `tokio::spawn`
  (which is `Send`-bound, unwatched and outlives the link).
- Reports go through the `Reporter`; nothing waits on MQTT.
- A target or property name must be one topic segment (no `/`, `+`, `#`, not empty): one that is not is
  dropped, with an ERROR `driver_contract_violation`.
- `execute` acts once.
- Every guard has a test, and every test has been seen to fail with the guard removed.

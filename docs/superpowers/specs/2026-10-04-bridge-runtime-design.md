# Bridge Runtime (`mqtt-bridge-kit`) — design

**Status:** approved by the owner, 2026-10-04. R1 built; its gate's findings fixed (§5.2 and the
notes through §5.1, §12), 2026-10-04.
**Companions (kept in sync, owner rule 2026-10-04):**
- the SDL Bridge Support Kit spec (Store, `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md`),
  which owns the wire contract;
- the Bridge Kit Tutorial;
- the `developing-mqtt-bridges` skill.

A change to any of them updates the others in the same change. This spec is the source for the bridge
side; the artifacts follow it:
- runtime: https://claude.ai/artifact/VtZNayRupmyBtUdcaWbqY3
- tutorial: https://claude.ai/artifact/23pVXtj2orqEXfGKL9D7zx

## 1. Why

Each of the seven device bridges carries its own copy of the same machinery, and no-hang 3b (Store spec
§14.8–§14.10) spent three two-family review rounds finding bugs in those copies:

- **The MQTT pump:** the rumqttc 0.25 collision fix, the clean-start republish, and the backlog
  episode.
- **Outages:** episodes with proof-of-life recovery and paced reconnects; two bridges had hot loops.
- **The device mailbox:** state held latest-per-target, momentary commands refused, reads dropped.
  Held state was lost in two bridges.
- **Reporting:** the device side must never wait on MQTT.
- **Start-up and shutdown:** signals first, the logging start raced against the stop, a bounded process
  end, no blocking prints.

The Bridge Kit adds the `Desired` lane, expiry, conflict resolution, `Event`, and replies to every bridge.
Written once, the policy gets one review, one test suite and one fix; each bridge keeps only its device
protocol.

## 2. Decisions (2026-10-04)

- **D1, shape:** one Rust runtime, with drivers in-process (a Rust trait) or as a plugin process speaking
  a stdio protocol (any language). The policy is written and reviewed once. A native runtime in another
  language stays possible later, from the same API and conformance suite.
- **D2, home:** this workspace, next to `mqtt-test-broker`, `wait-lint` and `lock-order`.
- **D3, order:** R1 lands with Bridge Kit S1/S2, with mqtt_ac v2 as its first user. The other Rust bridges
  follow with their Bridge Kit S4 steps. The stdio binding and Python SDK (R3) come when the first Python
  bridge is needed.
- **Install base:** mqtt_ac, mqtt_dmx and mqtt_dali serve the old home system, so their runtime versions
  are a new, incompatible v2 on `main`; v1 stays on a `v1` branch. Lutron, HDL and YNCA move in place.

## 3. Layers

1. **Wire contract:** the Bridge Kit grammar, plus JSON Schemas for each lane's payload, shared with the
   Store kit's tests.
2. **Driver API:** language-neutral. Every operation has a meaning, a deadline and an error class. It is
   not code-generated.
3. **Bindings:** the same API twice: a Rust trait (in-process) and a stdio protocol (a plugin process,
   JSON lines).
4. **Runtime:** everything that is policy, in Rust.
5. **Conformance suite:** the executable contract, a black-box suite over MQTT that runs against either
   binding with a scriptable fake device.

## 4. The driver API

A driver is one object, called by one runtime task at a time, so a device protocol that needs strict
sequencing gets it for free. Every call carries a deadline the runtime enforces; an overrun counts as a
lost link, never a hang.

### 4.1 Operations the driver implements

| Operation | Called when | Must | Returns |
|---|---|---|---|
| `info()` | once, at start | be pure | `DriverInfo` (§4.4) |
| `connect(ctx)` | to open the device link (paced retries) | finish or fail within the connect timing; may start helpers via `ctx` | ok or `LinkError` |
| `disconnect()` | after a lost link, at shutdown | be quick; best effort | nothing |
| `read_state(scope)` | after connecting, on the poll interval, for `refresh`, and before resolving outage conflicts | read the device, not a cache | observed state per target |
| `apply(target, values)` | for a request; several values arrive together for an atomic write | apply all values, in the order the device needs | ok, an error class for the whole write, or `Partial`: the properties that failed, each with its class (the rest were applied) |
| `execute(target, command, params)` | for a momentary command | act once; never retry | ok with an optional reply, or an error class |
| `configure(target, config)` | when a `Config` intent arrives or is retracted (`none`) | accept the setup | ok or an error class |
| `probe()` | for drivers with `writes: unacknowledged`: idle and while busy | prove the device is alive (Lutron's `DLMON`) | ok or `LinkError`; defaults to a read. `Unsupported` proves nothing: from an unacknowledged driver it is a contract violation (its writes are still taken as proven) |

### 4.2 The link context

- `ctx.report.state(target, values, assumed?)`: observed or assumed state, from an operation or a
  helper.
- `ctx.report.remove(target)` and `ctx.report.remove_property(target, property)`: a target gone
  from the device (its retained `State` cleared with an empty retained payload, after every CONNACK
  too, until it is reported again), or one property of it.
- `ctx.report.event(event)`: something that happened.
- `ctx.spawn(task)`: a helper the runtime owns (an unsolicited-input reader), for the life of the link:
  cancelled when the link drops, and watched — a helper that ends, by returning or panicking, is the
  link lost (a panic is also ERROR `driver_contract_violation`). Helpers run on the worker's own
  single-threaded runtime, as the driver does, so they need not be `Send`; `LinkHandle::spawn`
  starts one from a later operation or another helper (only on the worker's thread).
- `ctx.spawn_transient(task)` (and `LinkHandle::spawn_transient`): a helper that may end — a
  delayed re-query, a debounce timer. Cancelled with the link like any helper; its return is not a
  lost link; a panic still is, and ERROR `driver_contract_violation` (re-gate R17).
- `ctx.link_lost(reason)`: a helper noticed the link died.
- `ctx.blocking(fn)`: a synchronous library call on a worker thread — called from an operation,
  under that operation's deadline (a task-local, set around each operation and the disconnect);
  called from a helper, a task of its own, with none, even while an operation runs (re-gate R14). An
  overrun abandons the thread and counts as a lost link. Every call's thread is counted from before
  it starts until it ends — in use or abandoned, a helper's or a cancelled caller's too — and at
  most four run at once; past that a call is refused as a lost link.
- `ctx.handle()`: a cloneable `LinkHandle` (`report()`, `link_lost()`, `is_lost()`, `blocking()`, `spawn()`) to
  keep for later calls, since the context itself is only handed to `connect`. `ctx.report` is a
  cloneable `Reporter`.

Every reporter call returns at once and never waits on MQTT. State coalesces per topic; events go to a
capped queue whose overflow is counted and logged once per episode.

### 4.3 Errors

| Class | Means | The runtime then |
|---|---|---|
| `Link` | the connection failed: I/O, timeout, closed | drops the link; starts or continues the outage episode; holds state requests, refuses momentary ones; reconnects at the paced rate |
| `Rejected` | the device refused this request | publishes `Error` naming the target and property (the Store abandons it); WARN `command_rejected`; the link stays up |
| `Unusable` | a reply the driver couldn't parse | publishes `Error` once and passes the request over; never reconnects for it (3b: that was a hot loop) |
| `Unsupported` | no such command or property | publishes `Error`; no retry |
| `Partial` | (`apply` only) these properties failed, each `Rejected`, `Unusable` or `Unsupported`; every other value was applied | publishes `Error` for each failed property alone (the rest stand as applied, unproven for unacknowledged writes); WARN `command_rejected` for a rejected one; the link stays up. In the plugin protocol: `{"error":{"class":"partial","failures":[{"property":…,"class":…,"message":…}]}}` |

### 4.4 Capabilities (`DriverInfo`)

- **`writes`:** `acknowledged` (a successful `apply` is final: the device confirmed it, CoolMaster,
  or nothing more can ever be learned of it — a one-way transport with nothing to probe, DMX or DALI
  behind a write-only interface, declares this honestly and needs no `probe`), or `unacknowledged`
  (it only means "sent", and a later answer can prove it; the HWI, YNCA). With `unacknowledged`,
  each applied value stays unproven until a later successful `probe` or read, and is held again if
  the link dies first, unless a newer value waits; such a driver must probe, and a `probe`
  answering `Unsupported` is a contract violation (re-gate R17).
- **`feedback`:** `observed`, or `assumed` for one-way devices. This is the driver's default; a target's
  `Config` may override it. With `assumed`, the runtime publishes applied values as `State` marked
  `"assumed": true` unless the driver reports otherwise.
- **`commands`:** each with a class: `momentary`, `read` (coalesced; dropped while down), or
  `with_reply`. `refresh` (a `read_state`) and `assume` (set the assumed state without calling the
  driver) are standard and handled by the runtime.
- **`timing`:** defaults for connect, operation, retry pacing, the outage WARN threshold (30 s), the poll
  interval, and the probe interval and deadline. Each can be overridden on the command line.
- **`restore`** (default off; `--restore on|off` overrides it, `Bridge::restore` in code): at the
  process's first connect, each target with assumed feedback gets the read-back's values — what the
  previous run said it had set — after the held configs, before any held value, once the read-back
  is in (at most 10 s). For a one-way device that comes back dark (DMX). A property a newer request
  waits for is left to that request.

## 5. What the runtime owns

| Area | Behaviour |
|---|---|
| Topics | the grammar from `--root` and `--instance`; subscriptions to `Desired` (target-level `…/+` and `Command` retain-as-published; property-level `…/+/+` not), `Command`, `Config` |
| Pump | polls only; unbounded forward with a backlog episode; one connection per client: the first failed poll ends the session, and the next connection gets a fresh client (rumqttc's in-loop reconnect would send what the session queued meanwhile ahead of the next CONNACK's subscriptions, and carries the 0.25 packet-id collision across connections); progress bounded at twice the keep-alive (§5.2) |
| Every CONNACK | subscribe, then `Active=true`, then republish the retained model; `Version` |
| Start-up read-back | reads its own retained `State`, for the conflict rule's "before" values after a restart; the requests are subscribed only once its SUBACK has come (at most 5 s), so its retained messages are queued ahead of every retained request |
| Requests | decodes both `Desired` forms; the deadline from the MQTT message expiry, counted from the request's arrival (the pump stamps it); `conflict` from the user property; retractions; malformed payloads answered with `Error` |
| Mailbox | latest per (target, property), moved to the back; momentary FIFO (cap 64) refused while down; reads coalesced; held values dropped at their deadline; unproven writes held again |
| Worker | connect with pacing; proof of life before declaring recovery; catch-up (read, resolve conflicts, apply held state); poll; probes |
| Reports | a never-waiting model for `State` and `Status`; capped queues for `Event` and `Error` |
| Replies | a command's answer goes to its MQTT v5 response topic, carrying its correlation data |
| Outages | broker and device episodes: INFO first failure, DEBUG retries, one WARN past 30 s, INFO recovery with `down_for_ms` and `attempts` — declared once a connection (broker) or a recovered link (device, `Timing::stable`, 5 s, `--device-stable`) has lasted |
| Process | signals first (without them, the logging start is waited for at most 5 s, to say so, and the bridge does not start); logging start raced against the stop; no blocking prints (the logging's own failure is written by a thread of its own); `Active=false` and DISCONNECT; explicit runtime ended with `shutdown_timeout`; `ExitCode` |
| Logging | tracing-init with lossy destinations; the fleet's kind names (Store `docs/guides/logging-policy.md`) |

Device-link states: `Connecting` → `Proving` (connected, awaiting the first good read or probe) → `Up`,
or `Down` on `LinkError`, a missed deadline or `link_lost`. `Down` retries after the pacing interval.
Catch-up runs on entering `Up`. While `Down`, state requests are held to their deadline, momentary
commands are refused, and reads are dropped.

### 5.1 Wire details fixed while building R1 (2026-10-04)

- **Plain text, not JSON:** `Active`, `Status` and `Version` payloads (`true`, `connected`,
  `mqtt_ac 2.0.0`). Every other lane is JSON.
- **`Error` always carries `reason`:** one of `malformed`, `refused`, `rejected`, `unusable`,
  `unsupported`, `superseded`, `link_lost`. It may also carry `target`, `property`, `command` and an
  `error` text.
- **Replies are Event-shaped** (`"kind":"reply"`), sent to the response topic of any command that
  names one, with its correlation data. Failures also go to `Error`. Every terminal path of a
  command answers: refused as it arrives (malformed or retained; a command name that cannot be
  read is `?`), refused while the device is down or at a disconnect (reads included), expired
  while queued (`refused`), and a failed `refresh`. (No command path supersedes a command: a
  coalesced read answers every reply waiting on it.)
- **Request rules:**
  - A missing `conflict` user property means `DeviceWins`, the conservative choice: a manual change is
    never reverted on a guess.
  - A held value the device already has is satisfied: it is dropped, with no `Error`. JSON numbers
    compare by value (`24` and `24.0` are one; two integers exactly), in the satisfied test and the
    "changed" test alike.
  - A `Desired` replayed as retained gets the conflict check even while the device is up (the
    bridge-restart case).
  - **"Before" values, per value, by where it came from** (kit spec §4.5, "the last value it saw
    before the outage"): a value the broker replayed resolves against the model as it stood when
    the MQTT connection was lost (the first failed poll of a connection) — in the process's first
    session, against the read-back; a value held while the device was down resolves against the
    model as it stood when the device's last recovered link was lost — before any, the read-back;
    a value both replayed and held takes the older snapshot first. A device snapshot is taken only
    when a link whose catch-up was done is lost, so a link lost during its catch-up keeps the
    pre-outage values. The check applies to each value on its own: a live value taken with a
    checked one is applied as it is.
  - **Unproven assumptions:** for a target with assumed feedback, what the runtime assumes from
    an unacknowledged write is published as assumed `State` at once, but the conflict rule's
    current values and snapshots leave it out — they keep the value before it — until a probe or
    read proves that write (a report of the key settles it too). An unproven write held again
    when the link dies is then not satisfied by, nor superseded by, its own assumption, under
    either conflict policy (re-gate R3).
  - **What the snapshots hold:** the reported model, and over it what an acknowledged `apply`
    (`Writes::Acknowledged`) wrote since — for a `Partial`, only the values it did not name. A
    report of the key replaces a written value, and a key the driver reported during the `apply`
    keeps its report. Written values are never published as `State`: the device may have clamped
    them. A snapshot already taken is not changed by a later write.
  - **Current values:** for a target with assumed feedback, the model's (what the bridge
    believes); otherwise the proving read's, and a target it did not give (a device read one
    address at a time answers a read of all `Unsupported`) is read on its own. After the conflict
    read's wait, a value retracted, replaced or past its deadline meanwhile is dropped.
  - **The rule's rows, the first that holds deciding:** (1) the current value known and the
    requested one: satisfied; (2) no "before" value (a first install): applied, the current value
    known or not — nothing can have changed; (3) the current value unknown: under `DeviceWins` not
    applied but reported `superseded`, its `error` saying the current value is unknown (a current
    value still unknown is no reason to revert a person's change), under `RequestWins` applied;
    (4) changed since before: `DeviceWins` supersedes, `RequestWins` applies; (5) otherwise
    applied.
  - A retained target-level `Desired`, or a retained `Command`, is refused as `malformed` — replayed
    or live: both are subscribed retain-as-published, so the broker keeps the flag on a live
    delivery. Property writes are subscribed without it, so a live retained one (every Store write)
    is live, and only a replay arrives flagged.
  - A response topic on the bridge's own lanes is refused as `malformed`: on those it reads
    (`Desired`, `Command`, `Config`) the reply would be read back as a request; on those it
    publishes but `Event` (`State`, `Error`, `Status`, `Active`, `Version`) it would pass for the
    bridge's own report. So is one on any instance's request lane under the same root, where
    another bridge would take it for a request (re-gate R17). The `Event` lane stays open: replies
    are Event-shaped (`"kind":"reply"`), and the kit spec has them arrive there.
  - A target-level write's property copies are skipped for 60 s.
  - `configure` is called only while connected. Configs are held while down and applied first in the
    catch-up: every held config before any held value; while up, a target's waiting config before
    its values; `Config` is subscribed before `Desired`. A config's `feedback` override takes effect
    once `configure` has taken it. A config identical to the one `configure` last took on the
    current link is not handed over again (every reconnect replays the retained configs); a lost
    link forgets what was taken.
  - A momentary command still waiting at its message expiry is dropped — on `Error` (`refused`) and
    answered. Any command past its expiry when the session reads it (it waited behind a stalled
    broker) is refused the same way, `assume` and the reads included; a coalesced read keeps each
    caller's deadline, refuses each caller past it on its own, and runs only for the rest (re-gate
    R9).
- **Command line:** a zero `--retry`, `--probe-interval` or `--mqtt-retry`, a `--mqtt-keep-alive`
  under 5 s (rumqttc's floor) or over 65535 s (MQTT's 16-bit field), and any duration over a week
  (near the type's limit it overflows an `Instant`) are usage errors (exit 2); the same timings
  set in code fail the start, and so do a zero poll and a zero MQTT poll-retry. `--poll 0`, in any
  unit, and `--poll off` turn polling off.
- **Timing:** the busy-probe interval is a 2 s constant (from mqtt_lutron), counted from the oldest
  write still unproven — a newer value of a key keeps its older one's time. `Timing.poll` is an
  `Option`.
- **Kinds:**
  - `event_report_dropped` (WARN) and `event_report_drop_ended` (INFO);
  - `driver_contract_violation` (ERROR): an event without a string `kind`; a helper's panic; a
    reported target or property name that cannot stand as one topic segment (dropped, said once
    per name); a `probe` answering `Unsupported` from a driver with unacknowledged writes (once per
    process: it proves nothing, yet its writes are taken as proven, so it cannot become a probe
    hot loop);
  - `device_recovered` gains `expired`;
  - the runtime also emits `startup_failed` and `bridge_shutdown_cut`;
  - `panic` (ERROR), from the panic hook: `thread`, `location`, `payload`;
  - `read_back_incomplete` (WARN): the read-back's SUBACK did not come within 5 s; the requests
    were subscribed without it.

### 5.2 Bounds and repairs from the R1 gate (2026-10-04)

- **MQTT progress:** a request outstanding with nothing acknowledged for twice the keep-alive
  (10 s at the default) ends the session, and so does one poll that has not returned within it.
  The event loop is dropped, never polled again, and the paced retry makes a fresh client. A
  PINGRESP or a publish from the broker is not progress: a broker that answers PINGREQ and
  withholds its acknowledgements fills rumqttc's in-flight window, and rumqttc then takes no
  request while its pings succeed; a broker that stops reading holds rumqttc in its flush,
  inside the poll, where its keep-alive cannot fire. Reconnects are paced by `poll_retry`, and by
  `retry` after every fifth failed connect in a row.
- **No operation on a lost link:** an operation is refused at once, the driver not called, on a
  link already reported lost; one admitted while the link was live and completed keeps its result
  over a loss noticed at the same moment (re-gate R6).
- **A driver's `info()` that does not return:** `run` calls it on a blocking thread, raced against
  the stop signals taken before it, so a driver breaking "info is pure" cannot hold up the stop
  (Codex, `process.rs:251`).
- **A driver that blocks its thread:** its operation's deadline cannot fire on the worker's own
  runtime, so a watchdog on the MQTT side reads it (an atomic). The watchdog is a task of its own,
  started with the bridge, and keeps its verdict until it is read: an in-process user of
  `Bridge::start` gets the verdict, and the dead-part path, only by polling `Running::ended()`,
  but the device is said unreachable and the waiting commands refused at once either way. It
  declares the worker dead when:
  - an operation (or the disconnect) is past its deadline and a 2 s grace;
  - no operation runs and the worker runtime's heartbeat — a ticker on the worker's thread, every
    250 ms — has been silent for 5 s: a helper blocking the thread with a synchronous call;
  - the blocking calls' cap has refused calls, with no place freed, for twice the operation bound:
    threads stuck in synchronous calls for good, so every later call would be refused.

  Then: ERROR `worker_died`, `Status=unreachable`, the momentary commands waiting refused, and the
  dead-part path — the bounded shutdown (which does not wait for the blocked worker) and a failure
  exit, so the supervisor restarts the process. A thread cannot be cancelled; the restart is the
  recovery (re-gate R1, R13).
- **Panics:** `Bridge::run` installs a process-global panic hook (`install_panic_hook`, also for
  in-process users of `Bridge::start`): ERROR `panic` through tracing (lossy), and the report for
  stderr handed to the process's one reporter thread through a bounded, lossy queue (16; full, the
  report is dropped), waited for at most 200 ms — so repeated panics with a stderr nobody drains
  hold one thread, not one apiece (re-gate R8). Nothing is written on the
  panicking thread, so a panic with a stderr nobody drains still becomes a dead part — the device
  worker's `catch_unwind`, or the MQTT task's end, whose connection then closes and whose last
  will says `Active=false`.
- **Message size:** the bridge advertises a maximum packet size of 1 MiB in its CONNECT, so a broker
  discards a larger message for it instead of sending it (rumqttc enforces 10 KiB by default
  without saying so, and one large retained message made a reconnect loop). 1 MiB matches the
  plugin protocol's line cap.
- **One message, not the session:** a publish rumqttc refuses is that message dropped (DEBUG), and
  the CONNACK duties and every later publish go on; a reported name that cannot be a topic
  segment is dropped where it is reported.
- **Succeeded, then the link died:** an operation that has completed wins over a `link_lost`
  noticed at the same moment; its result stands, and the loss is seen at the worker's next step.
- **Polls:** the next poll is due an interval after the last one ended, and one waiting request is
  served between polls, so a poll slower than its interval cannot starve the writes.

**Out of scope:** airzoned (a media daemon) keeps its own structure. mqtt_mdns adopts only the pump,
reports and process pieces, since it has no device link.

## 6. Rust binding

```rust
pub trait DeviceDriver: Send + 'static {
    fn info(&self) -> DriverInfo;
    async fn connect(&mut self, ctx: &mut LinkContext) -> Result<(), LinkError>;
    async fn disconnect(&mut self);
    async fn read_state(&mut self, scope: Scope) -> Result<Vec<TargetState>, OpError>;
    async fn apply(&mut self, target: &str, values: &PropertyMap) -> Result<(), OpError>;
    async fn execute(&mut self, target: Option<&str>, call: &CommandCall) -> Result<Option<Value>, OpError>;
    async fn configure(&mut self, target: &str, config: Option<&Value>) -> Result<(), OpError>;
    async fn probe(&mut self) -> Result<(), OpError> { self.read_state(Scope::Any).await.map(drop) }
}

pub enum OpError { Link(LinkError), Rejected(String), Unusable(String), Unsupported(String), Partial(Vec<PropertyFailure>) }

fn main() -> ExitCode {
    mqtt_bridge_kit::Bridge::new("Aircondition").from_cli().run(Coolmaster::new())
}
```

- Native `async fn` in the trait needs `#[allow(async_fn_in_trait)]`, and its futures are not `Send`,
  so the runtime cannot `tokio::spawn` them. The driver therefore runs on a thread of its own with a
  single-threaded runtime (as built in R1). A driver that blocks its thread holds up neither the MQTT
  side nor the stop, and past its operation's deadline it is a dead worker (§5.2).
- `Bridge` also has `option(name, help)` / `value(name)` for driver-specific options (mqtt_ac v2's
  CoolMaster address), `require(name)` for one the command line must give, `usage_error(message)`
  for a value the driver cannot use, and `usage_exit()` — `--help` or a usage error said, and its
  exit code, before any driver is built; and `start(driver) -> Running` to run a bridge in-process
  (used by the suite).
- The stdio binding is itself a `DeviceDriver` (`PluginDriver`) that forwards each call to the plugin
  process.

## 7. Stdio plugin protocol

The host binary runs a plugin and talks to it over stdin/stdout, one JSON object per line:
`mqtt-bridge-host --root IR --instance living --broker … --plugin "python3 -m broadlink_bridge"`.

```text
→ {"id":1,"op":"hello","api":1}
← {"id":1,"ok":{"info":{...}}}
→ {"id":2,"op":"connect","deadline_ms":10000}
← {"id":2,"ok":null}
→ {"id":3,"op":"apply","target":"tv","values":{"power":true},"deadline_ms":3000}
← {"id":3,"ok":null}
← {"note":"event","event":{"kind":"code","code":"…"}}
→ {"id":4,"op":"execute","target":"tv","command":"learn","params":{"key":"Netflix"},"deadline_ms":30000}
← {"id":4,"error":{"class":"rejected","message":"no IR heard within 30 s"}}
```

**The host enforces:**
- **Draining:** stdout is read on a task of its own and always drained. stderr is drained to the log,
  lossy.
- **Deadlines:** writes to stdin are bounded. A plugin that stops reading, or misses a deadline, counts
  as a lost link.
- **Crashes:** a plugin that exits is an outage episode and is restarted at the paced rate. One that
  ignores `disconnect` is killed with its process group.
- **Message size:** lines are capped at 1 MiB. An oversized or malformed line is `Unusable`.
- **One request at a time,** unless the plugin declares `"concurrent": true`.

## 8. Python SDK

The `mqtt_bridge_kit` package gives Python the same API shape:
- a `DeviceDriver` base class with the eight operations as `async` methods;
- the four error classes as exceptions;
- a `LinkContext` with the same calls;
- `run_plugin(driver)` to serve the protocol.

The motivating case is an IR blaster driven by the `python-broadlink` library. The plugin has no MQTT
client and no outage or shutdown logic; it inherits them from the host.

## 9. Conformance suite

One black-box suite plays the Store's side over MQTT (`FakeBroker`) and drives a scriptable fake device.
The fake device is itself a driver, in-process for the Rust binding and a plugin for the stdio binding,
so both bindings run the same suite.

**Scenarios:**
- **Reconnects:** a CONNACK (subscriptions before `Active`, then the model republished); a broker
  restart without persistence.
- **Requests:**
  - a request while the device is down, applied on recovery within its expiry and never after it;
  - a momentary command while down (`Error`; never applied later);
  - the outage conflict both ways;
  - a target-level write reaching `apply` once with all values;
  - a retraction before the device returns.
- **Driver errors:**
  - `Rejected`, `Unusable` and `Link` with their behaviours, including paced reconnects (at most one per
    retry interval);
  - unacknowledged writes held again when the link dies before a probe;
  - a driver call that never returns, which becomes a lost link within its deadline.
- **Process:** SIGTERM, including during a stalled `connect`; malformed `Desired` and `Command`
  payloads.
- **Plugins:** a plugin that crashes, hangs or floods its stdout.

Every guard also gets a negative-control row. The suite runs in this repo's CI.

## 10. Repository layout

```text
mqtt-bridge-kit/          the runtime and the Rust trait binding (library)
  src/  topics · wire · pump · session · mailbox · worker · reports · replies · process · plugin
mqtt-bridge-host/         binary: runs a stdio plugin under the runtime
mqtt-bridge-conformance/  the suite; the fake device as a driver and as a plugin
python/mqtt_bridge_kit/   the Python SDK (stdio binding only)
schemas/                  JSON Schemas for each lane's payload
```

## 11. Rollout

- **R1** (with Bridge Kit S1/S2): the runtime, the trait and the suite, lifted from 3b's code:
  - mqtt_ac's mailbox and reports;
  - lutron's probe accounting;
  - ynca's order-based proof;
  - the shared pump and process pieces.

  **mqtt_ac v2** is its first user: the CoolMaster protocol as a `DeviceDriver`.
- **R2** (with Bridge Kit S4): Lutron, HDL and YNCA in place; DMX and DALI as v2.
- **R3:** `mqtt-bridge-host`, the protocol, the plugin fake device, the Python SDK, and a Broadlink IR
  plugin.

## 12. Risks

- **One runtime is one point of failure.** Mitigated by the conformance suite, negative controls,
  two-family review, and staged migration, so a regression shows on one pair first.
- **The API may not fit an odd device.** A device that streams, or needs concurrent operations, strains
  the one-call-at-a-time model. The outlets are `ctx.spawn` and the plugin `concurrent` flag; R2's five
  drivers test them.
- **Blocking libraries.** A synchronous call can only be abandoned, not cancelled. `ctx.blocking` bounds
  it and caps the number abandoned.
- **A plugin is a second process to deploy.** The host supervises it.
- **The read-back can miss.** A connection lost before the read-back's SUBACK leaves it to the next
  session, which subscribes again; but past 5 s without the SUBACK the requests are subscribed
  without it (WARN `read_back_incomplete`), and a replay that meets no "before" values is applied.
  After a broker-only outage, replayed
  requests are checked against the model as it stood when the connection was lost; the read-back
  serves only the process's first session (and held values until the device's first outage
  snapshot). A change at the device the bridge never saw — made while the bridge was away, between
  two reads — is still a change.
- **The read-back's order rests on the broker.** The requests are subscribed once the read-back's
  SUBACK has come, so a broker that writes a subscription's retained messages right behind its
  SUBACK (Mosquitto does) queues the retained `State` ahead of every retained request. A broker
  that sent retained messages later, on its own schedule, could still interleave them; and past
  5 s without the SUBACK the requests are subscribed anyway (WARN `read_back_incomplete`).

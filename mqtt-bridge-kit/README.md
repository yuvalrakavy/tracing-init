# mqtt-bridge-kit — the Bridge Runtime

One Rust runtime that owns every MQTT device bridge's *policy*, so that each bridge keeps only its
device protocol, written as a `DeviceDriver`. The pump, the outages, the queue of requests while the
device is down, the conflict rule, the reports, the replies, the logging and the process are written
once, reviewed once and tested once.

```rust,ignore
use std::process::ExitCode;
use mqtt_bridge_kit::Bridge;

fn main() -> ExitCode {
    Bridge::new("Aircondition").from_cli().run(Coolmaster::new())
}
```

A device-direct bridge, many devices in one process, is a `Host` instead (see the topology, below):

```rust,ignore
Host::new("ynca").root("Ynca").from_cli().device(receiver_from_config).run()
```

- **Learn it:** [`docs/tutorial-writing-a-bridge.md`](docs/tutorial-writing-a-bridge.md) writes a
  complete bridge step by step, with tests, negative controls and a systemd unit. Its code is
  [`examples/tutorial_bridge.rs`](examples/tutorial_bridge.rs); its §12, a multi-device host, is
  [`examples/tutorial_multidevice.rs`](examples/tutorial_multidevice.rs).
- **Look it up:** `cargo doc --no-deps --open` (every public item is documented), and this page.
- **The source of truth:**
  [`docs/superpowers/specs/2026-10-04-bridge-runtime-design.md`](../docs/superpowers/specs/2026-10-04-bridge-runtime-design.md)
  (the runtime) and
  [`docs/superpowers/specs/2026-10-10-multi-device-host-design.md`](../docs/superpowers/specs/2026-10-10-multi-device-host-design.md)
  (the multi-device host), and, for the wire contract, the Store's Bridge Kit spec
  (`docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md`) and contract guide
  (`docs/guides/mqtt-bridge-contract.md`) in the Store repository.
- **A real bridge on it:** [mqtt_ac v2](https://github.com/yuvalrakavy/mqtt_ac/tree/265df22) (the
  CoolMaster air-conditioning controller).

The crate is not published (`publish = false`); bridges depend on it by git revision and pin one.
Minimum Rust version 1.80.

## Process services that must survive link outages

A controller may also serve a local configuration UI or own a shared bus. Those
resources belong to the process: a failed MQTT connection or a device reconnect
must not stop them. Implement `ProcessService` and use
`Bridge::run_managed(service)` (`start_managed` for embedding/tests).

Construct the service without opening resources. Its async `run` owns listeners,
bus handles and helper tasks on an isolated current-thread Tokio runtime. Once
ready, call `context.ready(driver)?` to transfer a resource-free driver proxy.
The runtime validates its pure `info()` once before starting device/MQTT work.
Then serve until `context.cancelled().await`, close admission and release the
service resources. Driver `disconnect` affects the device link only. Service
shutdown must not wait for MQTT disconnection.

`ServiceTiming` defaults to 10 s startup and 2 s cooperative stop. A failed,
panicked or synchronously blocked service is a component death; its independent
watcher closes MQTT admission even if an embedder does not poll `Running::ended`.
Service, device and MQTT teardown share one absolute 5 s ceiling. Forced
cancellation is successful only after the service future, local tasks and
runtime (including blocking helpers) are destroyed. A blocked thread/destructor
is abandoned at the parent deadline; `stop` returns false and managed process
shutdown exits unsuccessfully. Dropping a startup future or `Running` forces
cancellation, but only awaiting bounded stop confirms cleanup.

Service helpers must propagate fatal errors to `run`; an unobserved task panic
does not automatically fail the service. Keep resources inside `run` or its
runtime-owned tasks: detached OS threads need an explicit lifetime/bound policy.
The full contract is [managed process services](../docs/superpowers/specs/2026-10-21-managed-process-services.md).

## Bounded MQTT buffering

A fixed `Bridge` can opt into `buffers(BufferLimits { incoming_bytes, reply_bytes })`.
Both limits must be positive. Existing bridges retain their defaults unless they opt in.
Incoming accounting covers queued PUBLISH contents and decoded MQTT metadata, including
repeated user properties. Terminal-reply accounting follows each reply or request Error
through Reports, sealed output, the client request channel and the exact packet's PUBACK.
Enqueue alone does not free reply capacity. Successful and negative acknowledgments both
end that packet's reservation.

Exhaustion ends and retries only the MQTT connection, including when a publish is already
waiting for the client request channel. The managed service and its timers keep running.
The connection closes without a shutdown flush that could wait on the exhausted queue;
the broker's last will supplies offline status. Unanswered calls may time out and queued
answers may be abandoned. Committed engine changes are not rolled back. The retry logs
the exhausted queue; local buffering pressure is separate from broker unavailability.

Reservations release when queued input is dispatched/dropped or the reply's exact
packet is acknowledged. Connection loss discards queued answers; the next connection
clears the old in-flight reply table (final coordinator drop clears it too).
Connection capabilities prevent an old
pump from exhausting or crediting a new connection's budget. Cloned builders allocate
independent buffering state when started.

`Bridge::request_queue_bytes(limit)` separately bounds accounted request contents in
the fixed device's mailbox. The positive limit includes pending and in-flight state,
configuration and command parameters, coalesced read callers, and copy/conflict-basis
caches. Accounting uses the larger of encoded JSON and decoded contents, plus keys,
request metadata and fixed entry overhead; it is not an allocator or RSS measurement.
The default is unchanged when this option is absent.

An update is checked as a complete candidate: replacing a value credits the superseded
contents, and refusal leaves earlier work and its assumptions intact. The worker never
waits for space. A refused command/read receives a request Error and any correlated
reply; mailbox pressure alone does not reconnect MQTT or restart the managed service.
If those terminal answers exhaust the separately configured reply budget, the MQTT
retry policy above applies. Driver-owned allocations and temporary ingress parsing
are outside the mailbox budget and need their own bounds.

With `Bridge::packet_limits()`, queued output and direct lifecycle PUBLISH packets are checked before enqueue
against both the local 1 MiB ceiling and the current broker's advertised maximum.
Sizing includes the topic, QoS 1 packet id, properties/correlation and MQTT length
headers. An oversized message logs `mqtt_packet_rejected`, drops its reservation
and cannot receive a publish/cleanup receipt. Following smaller messages continue.

SUBSCRIBE and UNSUBSCRIBE use the same effective ceiling. If a required control
cannot fit, the connection latches a size fault: admission/output close, Active is
cleared when publishable, and the managed engine keeps running. There is no size-
driven reconnect loop or expired-timer spin. An actual later connection resets the
fault and uses its newly advertised ceiling. Repair the broker ceiling or topic
configuration before reconnecting. The pre-CONNACK CONNECT cannot use a ceiling
the broker has not advertised yet; keepalive and disconnect remain protocol control.
This policy is opt-in so existing bridges retain their outgoing-packet behavior.
Recovery snapshots always retain their existing explicit size checks.

`Bridge::strict_json()` rejects duplicate object keys in ordinary Config, Desired
and Command payloads before generic JSON decoding discards that distinction.
It checks nested objects and decoded key names, so `"x"` and `"\u0078"` conflict
within one object. Keys may repeat in different objects. The check visits values
without building a second JSON tree; it adds a parsing pass and keeps each object's
keys during that visit. Empty retractions retain their existing meaning. This
option does not impose driver schemas, canonical ordering or interpretation of
JSON encoded inside strings. Recovery documents remain the owning engine's
validation responsibility. The default remains unchanged for existing bridges.

These are accounted queue byte limits, not a process resident-memory measurement.
Request-mailbox limits, model/unsolicited-report storage and aggregate engine/recovery
allocation admission remain separate responsibilities; this API does not establish them.

## What the runtime owns, and what a driver writes

| The runtime owns | A driver writes |
|---|---|
| the topic grammar from `--root` and `--instance`; the subscriptions | how to open and close the device link (`connect`, `disconnect`) |
| the MQTT pump: one connection per client, polled only, progress bounded | how to read the device (`read_state`) |
| every CONNACK: subscribe, `Active=true`, `Version`, the retained model again | how to write it (`apply`), with `Partial` for per-property failures |
| the start-up read-back of the bridge's own retained `State` | how to act once (`execute`) and set a target up (`configure`) |
| requests: both `Desired` forms, expiry, `conflict`, retractions, malformed payloads | how to prove the device alive when it cannot acknowledge a write (`probe`) |
| the mailbox: latest per (target, property), momentary FIFO, held values with deadlines | a `DriverInfo`: acknowledged or not, observed or assumed, commands, timing |
| the worker: paced connects, proof of life, catch-up with the outage conflict rule, polls, probes | helpers for unsolicited input (`ctx.spawn`) |
| reports that never wait; capped `Event` and `Error` queues | what a target's `State` and `Event`s are (`ctx.report`) |
| replies on the response topic, with the correlation data | |
| outage episodes, with the fleet's log `kind` names | |
| the process: signals, logging start, bounded shutdown, panic hook, a watchdog on the device worker, the exit status | |
| the command line, `--help`, usage errors | the driver's own options (`Bridge::option`, `Host::option`) |
| (a `Host`) the instances: `Config/{instance}`, ownership, per-instance bootstrap, stopping, handover, the `via` tags, the publication rotation | the factory: a driver from an instance's Config, or a `FactoryError` |

The contract between the two is the `DeviceDriver` trait: every operation has a meaning, a deadline
the runtime enforces, and an error class that says what the runtime does next.

| Error class | Means | The runtime then |
|---|---|---|
| `Link` | the connection failed: I/O, timeout, closed | drops the link, starts or continues the outage episode, holds state requests, refuses momentary ones, reconnects at the paced rate |
| `Rejected` | the device refused this request | publishes `Error` (target, property); WARN `command_rejected`; the link stays up |
| `Unusable` | an answer the driver could not parse | publishes `Error` once and passes the request over; never reconnects for it |
| `Unsupported` | no such command or property | publishes `Error`; no retry |
| `Partial` | (`apply` only) these properties failed; every other value was applied | publishes `Error` for each failed property alone; the link stays up |

## Topology: `Bridge` or `Host` (owner decision D1)

An `{instance}` in the topic grammar is *one device link*: a controller the bridge fronts, or one
device it talks to directly. The runtime builds both shapes, and they are one runtime: a `Bridge` is
a `Host` with one fixed instance (below), over the same device machinery (R1's worker, mailbox,
reports, outage episodes, conflict rule):

- **A controller-fronted bridge: one process per controller, `Bridge`.** The device you talk to is a
  controller or gateway (a CoolMaster, an HWI, an AV receiver); the devices behind it are its
  **targets** (an indoor unit `L7.400`, a lamp zone). `--instance` names the controller, one process
  serves one device link, and its one driver is that link. Several controllers are several processes,
  each with its own instance. Nothing on the wire names the process: no `Config/{instance}`, no `via`
  tag. mqtt_ac is the worked example: one CoolMaster controller, its indoor units as targets.

  ```rust,ignore
  Bridge::new("Aircondition").from_cli().run(Coolmaster::new())
  ```

- **A device-direct bridge: one process, many devices, `Host`.** Each device is its own network
  endpoint (an IR emitter per room, mqtt_ynca's receivers), and one process serves many of them over
  one MQTT session. The process has a **bridge id** (`--name`, default the kind), unique on the root
  (a Config that omits `via` names the default id of the kind it names in `bridge`);
  it learns its instances from the Store's retained `{Root}/Config/{instance}` =
  `{"bridge": "<kind>", "via": "<bridge id>", …driver fields…, "disabled"?: bool}` and serves every
  instance whose Config names it, building each device with the bridge's **factory** from that Config.

  ```rust,ignore
  Host::new("ynca").root("Ynca").version("2.0.0").from_cli()
      .device(|config: &DeviceConfig| receiver_from(config.fields()))
      .run()
  ```

  Per instance, the host subscribes the instance's own lanes in three steps (its read-back, its target
  Configs, its requests), builds the driver off to the side within `--factory-timeout`, and starts the
  device once both are done; the device then has its own `Status/{instance}`, its own outage episodes
  and its own budgets. A Config change stops the old device (its in-flight requests answered, its
  output published) before the new one starts; `"disabled": true` stops it and publishes
  `Status=disabled`; a deleted Config stops it and retracts the instance's retained topics this
  process wrote; a Config whose `via` names another process hands the instance over. A factory that
  refuses a Config (`FactoryError::Malformed` / `Unusable`) publishes `Error/{instance}` and keeps the
  last accepted Config in force (none: the instance is *failed*, `Status=unreachable`, retried at the
  `--retry` pace); a `disabled` Config with changed driver fields is checked by the factory first. A
  cleanup that cannot be done at once — the delete ended while the broker was away, or the process
  was down — is done after a later root replay, re-reading the instance's slot so only values still
  tagged with this process are cleared.

  The failure domain is the process for a *blocked* device only (owner decision D4 = a): a device
  worker blocked past its deadline, a blocking budget that stays exhausted, or a factory or `info()`
  that does not return in time ends the process by R1's dead-worker path (bounded shutdown, failure
  exit, the supervisor restarts it). Every other device failure is that device's lost link; the other
  devices do not notice.

  The tutorial's [§12](docs/tutorial-writing-a-bridge.md#12-many-devices-in-one-process) writes one,
  in [`examples/tutorial_multidevice.rs`](examples/tutorial_multidevice.rs). The design is
  [`docs/superpowers/specs/2026-10-10-multi-device-host-design.md`](../docs/superpowers/specs/2026-10-10-multi-device-host-design.md)
  (§3 the wire contract, §4 the host).

Which to choose: a controller with its devices behind it is a `Bridge` (its targets are the devices);
devices that are each a network endpoint, configured by the Store, are a `Host`. Never put many
endpoints behind one `Bridge` driver: one stuck endpoint would then be the whole bridge's outage.

### One runtime: a `Bridge` is the host's N = 1 mode

There is one runtime, not two. A `Bridge` is a thin facade over `Host` with one **fixed instance**:
named from `--instance`, served by the driver `main` hands to `run`, and owned by the process itself
(multi-device spec §4.1, §4.2). So the fixed instance has no `Config/{instance}` and no `Config/+`
subscription, no root barrier, no `via` tag, no healing or cleanup, and its `Active` is
`Active/{instance}`; its CONNACK duties are R1's, in R1's order (the read-back `State/{instance}/#`
once per process and alone, then its requests, then `Active=true`, the `Version` and the model). A
read-back refused, or unacknowledged past its 5 s wait, is said incomplete and the bridge goes on
without it — past the wait, from a new connection: the session ends there, since only a new
connection frees the abandoned SUBSCRIBE's packet id.
Everything else — the session, the per-instance lifecycle that carries those duties, the publication
rotation, the packet-id budget (MQTT §2.2.1: no SUBSCRIBE, UNSUBSCRIBE or publish takes an id still
awaiting its acknowledgement) and the bounded shutdown flush — is the host's own code, the same for
one fixed device as for many configured ones. A `Bridge`'s API, command line and wire are unchanged.

## The topic grammar and lanes

The wire contract is the Store's Bridge Kit; the runtime implements the bridge side. Read it in the
Store repository: the spec `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md`, §3 (grammar
and lanes) and §4 (the `Desired` lane, conflicts), and the guide `docs/guides/mqtt-bridge-contract.md`.
This is the summary; [`topics`](src/topics.rs) builds every topic.

```text
{Root}/{Lane}/{instance}[/{target}[/{property}]]
```

`Root` (the device family; `--root`, default the family given to `Bridge::new`, or a `Host`'s kind),
`instance` (`--instance`; a `Host`'s from each `Config/{instance}`), a target and a property are each
**one topic segment**: never empty, never containing `/`, `+` or `#`. QoS 1 everywhere. A `Host`
publishes `Active` and `Version` under its bridge id (`--name`) instead of an instance, and every
`Status` and `State` it publishes carries the MQTT v5 user property `via` = its bridge id; a `Bridge`
publishes no `via`.

| Lane | Direction | Retained | Payload | Meaning |
|---|---|---|---|---|
| `Active/{instance}` (a `Host`: `Active/{bridge id}`) | bridge to Store | yes; last will `false` | plain text `true` / `false` | the bridge process is alive and connected |
| `Status/{instance}` | bridge to Store | yes; a `Host`'s tagged `via` | plain text `connecting` / `connected` / `unreachable`, and from a `Host` `disabled` | the device link |
| `Version/{instance}` (a `Host`: `Version/{bridge id}`) | bridge to Store | yes | plain text; a `Host`'s is `<kind> <version>` | the bridge's version (`Bridge::version`, `Host::version`) |
| `State/{instance}/{target}` | bridge to Store | yes (an empty payload retracts); a `Host`'s tagged `via` | JSON object | observed state; `"assumed": true` when not read back |
| `Event/{instance}` | bridge to Store | no | JSON `{kind, target?, ...}` | something happened; **replies** arrive here (`"kind":"reply"`) |
| `Error/{instance}` | bridge to Store | no | JSON `{error, reason, target?, property?, command?}` | a request refused or failed |
| `Desired/{instance}/{target}/{property}` | Store to bridge | yes (empty retracts) | JSON scalar | a requested value the device has not confirmed |
| `Desired/{instance}/{target}` | Store to bridge | never | JSON `{property: value, ...}` | several properties, atomically (one `apply`) |
| `Command/{instance}` | Store to bridge | never; 30 s expiry | JSON `{target?, command, ...}` | momentary actions; the standard `refresh` and `assume`; the driver's own |
| `Config/{instance}/{target}` | Store to bridge | yes (empty retracts) | JSON | per-target setup (to `configure`); may override the feedback mode |
| `Config/{instance}` | Store to a `Host` | yes (empty deletes the instance) | JSON `{bridge, via?, …driver fields…, disabled?}` | the instance: which kind, which process (`via`, default: the kind named in `bridge`), the factory's fields |

Details the runtime fixes (spec §5.1), which the lanes' payloads rely on:

- `Error` always carries a `reason`: `malformed`, `refused`, `rejected`, `unusable`, `unsupported`,
  `superseded` or `link_lost`.
- A request's deadline is its MQTT v5 message expiry, counted from the moment the bridge received it;
  its `conflict` user property (`DeviceWins` by default, the conservative choice, or `RequestWins`)
  decides what happens when the device changed while the request waited out an outage.
- A reply goes to the command's MQTT v5 response topic with its correlation data, shaped as an
  `Event`. A response topic on one of the bridge's own lanes, or on a request lane under the same
  root, is refused as `malformed`.
- A retained target-level `Desired` or a retained `Command` is refused as `malformed`.
- The bridge advertises a 1 MiB maximum packet size.
- JSON Schemas for each lane's payload are in [`../schemas`](../schemas); the conformance suite
  checks every publish against them.

### Optional request lifetime limit

`Bridge::request_lifetime_limit(Duration)` and `Host::request_lifetime_limit(Duration)`
opt into a queued-start deadline. For example, a DALI bridge uses
`request_lifetime_limit(Duration::from_secs(180))`. Without this call, existing
request handling and defaults remain unchanged.

With a limit, every nonempty property `Desired` needs an MQTT v5 message expiry
no longer than that limit, including live deliveries. Missing or longer expiry
is malformed and cannot replace a pending value. An empty retraction needs no
expiry. Target-level `Desired` and `Command` use the shorter of their existing
expiry and the limit; a missing expiry uses the limit. Shorter expiry is preserved.
The deadline starts at the pump's original receipt, so time in the session queue
counts. Configuration, State readback and already-started operation timeouts are
unchanged. Zero or an unrepresentable builder limit fails startup validation.

Publishers must set a bounded expiry when publishing property requests: the
broker's remaining expiry cannot reveal the original publication age. No publisher
timestamp or replay journal is added. This optional policy serves generic MQTT
consumers and is independent of Store.

Contextual driver methods receive this absolute monotonic expiry through
`RequestContext::deadline()`. Pass the original context through any driver-owned queue
and check it again before committing a side effect. Do not start another TTL at that queue.
An atomic write spanning several properties carries the earliest surviving deadline;
a coalesced read excludes expired callers before selecting its context and deadline.

### Optional request identity

MQTT v5 `request-id` identifies an operation independently of `traceparent`. If supplied,
it must occur once and contain exactly 64 lowercase hexadecimal characters (the publisher's
SHA-256 of its canonical operation). Invalid or duplicate identifiers reject the request.
The runtime validates the spelling; the driver owns operation-specific digest validation.
`RequestContext::request_id()` preserves the value across driver queues. Request `Error`
documents echo it as the optional JSON field `"request-id"`, including malformed requests
whose identifier itself is valid. Invalid or duplicate identifiers are never echoed.
Requests without it retain their existing wire behavior. Expiry and request identity do
not change generation authority, equality, hashing or mailbox coalescing keys.

## Retained recovery for managed bridges

`Bridge::recovery(RecoveryOptions { target, max_bytes })` opts a fixed managed bridge
into snapshot readback and exact model installation on every MQTT connection. Use
`start_managed` or `run_managed`. Its proxy must initially advertise a fenced policy
with an empty target table and a valid instance generation. Other bridges keep their
existing bootstrap.

The runtime subscribes to exactly `State/{instance}/{target}`, waits for that
subscription's successful SUBACK, then subscribes to a separate `_sync` nonce topic.
The second matched SUBACK closes replay. This requires a broker that orders earlier
retained replay before a later subscription acknowledgment. No retained marker is
written. Refused or overdue subscriptions end the connection and never prove absence.
The temporary subscriptions are removed afterwards.
The engine has ten seconds after replay to install its initial image. An overdue
decision ends and retries the MQTT connection while the managed engine keeps running.

`ServiceContext::recovery()` supplies a `RecoveryHandle`. Its `readback().await`
receives the latest completed replay once, with an opaque connection capability and
`Result<Option<Bytes>, String>`: absence, a bounded encoded payload, or an invalid
oversized replay. The engine validates its own snapshot format and chooses its recovery
policy. Race this wait with service cancellation; MQTT loss does not stop the service.

The engine calls `complete(connection, image, readiness, command_completion).await`.
An actuator loop can instead call synchronous `begin_complete(...)`, perform its
non-failing local activation, then select the returned ticket's `wait()` alongside
frame ticks and cancellation. Staging validates the current connection, original
deadline and any active command capability; the ticket confirms runtime installation,
not broker replication. Keep it alive through the result; a lost connection cannot
roll back lighting already committed locally.
If a command-triggered install later expires or loses its command capability after
local acceptance, preserve the failed/unknown command result and converge the
already-accepted local state with a fresh autonomous image on the **same original
connection**. Do not use a newer replay capability: its broker evidence still needs
the engine's normal selection. A lost original connection instead waits for that
normal replay; it does not roll lighting back.
`RecoveryImage` contains the complete generation table, assumed target model, accepted
target configurations, optional immutable snapshot and original request context. Installation replaces old state,
acknowledged writes, conflict/readback caches, setup caches and queued work, including
work whose mapping generation stayed unchanged. The original deadline is checked again
at installation. Trace causality is copied to the new model's own generations.
Only the exact initiating command capability can preserve its final completion.
The encoded model/setup seed is bounded at 1 MiB independently of the retained
snapshot's `max_bytes` limit.

Normal state and request admission wait for successful installation. `Ready` opens
ordinary requests; `RecoveryOnly` requires empty target/model/setup maps and allows
registered targetless recovery commands, which the proxy must validate. Builtin refresh
stays closed in RecoveryOnly; fenced assume stays unavailable. Refusals preserve a valid
command response topic and correlation data. An install abandoned before commit,
stale connection, invalid image or expired request cannot open the gates. RecoveryOnly
can promote to Ready on the current connection through another exact installation.
An explicitly declared `DiscoveryRead` command is a parameterless instance read. It
may omit generation to discover initial setup metadata; the runtime captures the
current generation and model cut once before queueing it. Supplied generations remain
subject to normal fencing. The driver must keep discovery read-only; no mutating
command gains this exception. Queued drivers can use `Reporter::request_is_current`
with the unchanged dequeued context immediately before an actuator commit. Exact
recovery replacement retires that context even when visible generations match.
Shutdown keeps output gated if no image was installed.
Keep the engine's staged installation job alive until its result is known, even if
the originating command caller disappears. Cancellation after commit cannot undo
an installed image; apply the committed engine candidate before answering later work.

Capture the complete encoded retained State carrier with
`RecoverySnapshot::new(revision, bytes, context)` and include it in the selected image.
The engine validates its snapshot schema; the runtime enforces the encoded byte cap.
After installation, `handle.publish(connection, snapshot)` replaces the one pending
snapshot without waiting for MQTT. It requires the current Ready connection;
RecoveryOnly cannot overwrite retained evidence. While disconnected, keep the latest
engine image and include its snapshot in the next exact installation.

Publication uses the existing session, packet-id budget and output rotation, with
one immutable snapshot in flight and one replaceable pending snapshot. Enqueues are
at least 100 ms apart. A delayed acknowledgment does not block the managed engine.
The snapshot keeps its explicit trace cause, including absence, without inheriting
an unrelated lifecycle trace or an array's target generation.

`handle.receipt()` returns the latest publication result. It names the exact
connection, process-local snapshot identity, engine revision and actual MQTT packet
identifier. Equal revisions do not imply equal snapshot identities. Successful
enqueue is not a receipt: acceptance requires the matching outgoing packet and a
successful PUBACK. A negative PUBACK is a fault. A complete snapshot packet exceeding
the local or broker-advertised maximum produces a fault with no packet identifier,
and does not cause a reconnect loop. Receipts reset at connection loss and exact
installation; they confirm broker acceptance, not broker disk durability.
This is snapshot-specific accounting; complete service-wide queue/memory limits
and packet admission for other publication kinds remain separate work.

The explicit broker acceptance test requires `mosquitto` on PATH and runs only on
an owned ephemeral loopback listener with persistence disabled:

```sh
cargo nextest run --manifest-path mqtt-bridge-kit/Cargo.toml --lib \
  --features mosquitto-acceptance -E 'test(isolated_mosquitto_orders)'
```

## Request traces across queues

MQTT v5 `traceparent` is optional diagnostic metadata. The runtime validates and
owns it in `RequestContext` across mailbox, worker, report and publication
boundaries. Invalid, duplicate or over-512-byte headers are ignored. Trace
metadata never changes generation admission, equality, hashing or coalescing.
An enabled INFO request carrier continues a supplied trace, or starts an ingress
root when OTel is installed. Without OTel, valid supplied headers still survive;
headerless requests fabricate no metadata. State readback remains DEBUG.

Contextual driver/report methods preserve that cause. Drivers crossing another
queue can instrument its future with `context.operation_span(instance, operation)`;
never hold a span enter guard over an await. Override
`read_state_with_request_context(scope, context)` to receive the original cause
for queued refreshes and individual conflict reads. Its default calls the
existing `read_state_with_context(scope)`, preserving existing drivers. Startup
proof and periodic polls use that existing hook. Capture returned observations'
mapping generations during actual resolution; the request token must not
relabel their authority. A merged atomic apply uses its latest
surviving request's cause, while property errors retain their own causes.
Each coalesced reply owns its caller's cause, including explicit absence.
`ReplyTo` now has a public `traceparent` field: migrate hand-built struct literals
to `ReplyTo::new(topic, correlation)` or supply that field. `from_properties`
captures wire callers automatically. Captured absence clears unrelated ambient
publication metadata; an unspecified constructor context retains legacy behavior.
See the [trace ownership design](../docs/superpowers/specs/2026-10-21-bridge-request-traces.md).

## The command line

`Bridge::from_cli()` reads these. The runtime owns them all; a driver adds its own with
`Bridge::option(name, help)` (`--name <value>`; `require(name)` makes one mandatory;
`usage_error(message)` rejects a value the driver cannot use). Durations are `500ms`, `10s`, `2m` or a
bare number of seconds, and at most a week. `--option=value` works as well as `--option value`.
`--help` (or `-h`) prints the usage and exits 0; a usage error prints it on stderr and exits 2, before
any driver is built.

| Option | Meaning | Default |
|---|---|---|
| `--instance <name>` | this bridge's instance (required) | |
| `--broker <host[:port]>` | the MQTT broker (required) | port 1883 |
| `--root <Root>` | the topic root | the family |
| `--log-config <path>` | a tracing-init `logging.toml` | built-in: a file under `logs/`, GELF |
| `--connect-timeout <d>` | the bound on a device connect | the driver's `Timing::connect` |
| `--operation-timeout <d>` | the bound on every other device operation | `Timing::operation` |
| `--retry <d>` | at most one device connect per this (not zero) | `Timing::retry` |
| `--outage-warn <d>` | a device outage this long is one WARN | `Timing::outage_warn` (30 s) |
| `--poll <d\|off>` | read the device this often; `0` or `off`: never | `Timing::poll` |
| `--probe-interval <d>` | probe a silent unacknowledged device this often (not zero) | `Timing::probe_interval` |
| `--probe-deadline <d>` | the bound on a probe | `Timing::probe_deadline` |
| `--device-stable <d>` | a recovered device link that lasts this long ends its outage | `Timing::stable` (5 s) |
| `--mqtt-keep-alive <d>` | MQTT keep-alive; 5 s to 65535 s | 5 s |
| `--mqtt-retry <d>` | the pause after five failed connects in a row (not zero) | 10 s |
| `--mqtt-warn <d>` | a broker outage this long is one WARN | 30 s |
| `--mqtt-stable <d>` | a connection that lasts this long ends a broker outage | 5 s |

Timings that cannot run (a zero pause, a keep-alive outside its range) are usage errors on the command
line and fail `Bridge::start` when set in code.

`Host::from_cli()` reads the same options, with two in place of `--instance` (a `Host` has no fixed
instance), and with its device timings applying to every device it builds:

| Option | Meaning | Default |
|---|---|---|
| `--name <id>` | this process's bridge id, unique on the root; the `via` its instances' Configs name | the kind |
| `--factory-timeout <d>` | the bound on one device's construction (the factory and `info()`); past it the process ends (D4) (not zero) | 10 s |

## The driver API

The reference is the rustdoc. In one screen:

```text
trait DeviceDriver: Send + 'static
  info()                            once, at start, pure                     -> DriverInfo
  connect(ctx: &mut LinkContext)    open the link; may start helpers         -> Result<(), LinkError>
  disconnect()                      after a lost link, at shutdown; quick
  read_state(scope)                 read the device, not a cache             -> Result<Vec<TargetState>, OpError>
  apply(target, values)             write; several values are one atomic write -> Result<(), OpError>   (or Partial)
  execute(target?, call)            act once, never retry                    -> Result<Option<Value>, OpError>
  configure(target, config?)        accept or withdraw a target's setup      -> Result<(), OpError>
  probe()                           prove the device alive (default: a read)  -> Result<(), OpError>
```

`DriverInfo` declares `writes` (`Acknowledged` or `Unacknowledged`), `feedback` (`Observed` or
`Assumed`), the `commands` (`Momentary`, `Read`, `WithReply`, or the opt-in `DiscoveryRead`)
and the `Timing` defaults. The
`LinkContext` given to `connect` has the `Reporter` (`state`, `event`, `remove`, `remove_property`),
the helper spawners (`spawn`, `spawn_transient`), `link_lost`, `blocking` (a synchronous call held to
the deadline), `read_back` (the previous run's retained `State`) and `handle()`, a cloneable
`LinkHandle` to keep for later operations. `Bridge` is the builder `main` uses; `Running` is a bridge
started in-process (`Bridge::start`), for tests and embedders.

## State equality with command annotations

`DeviceDriver::state_values_equal(target, property, left, right)` optionally defines
endpoint equality for outage resolution. The runtime uses it both to recognize an
already-satisfied request and to compare current state with its pre-outage baseline.
For example, an output string can carry a measured level and a remembered fade
duration; a driver can compare the level without treating the duration as a physical
device change. Reject malformed inputs rather than making them equal.

The default, also exported as `default_state_values_equal`, preserves numeric-value
comparison for JSON numbers and exact comparison otherwise. Delegate other properties
to that helper. The hook is pure, performs no I/O, and runs under the normal operation
deadline/watchdog. It changes neither wire values nor coalescing, expiry, tracing or
mapping authority. Existing drivers and stdio plugins retain the default. Request
validation still runs before comparison and must reject unsupported intent.

## Controllers whose target mappings can change

A fixed `Bridge` can opt into generation fencing with
`DeviceDriver::request_policy() -> RequestPolicy::Fenced(TargetGenerations)`.
The complete table contains an instance generation and every current target's
generation. The runtime captures it once with `info()` before requests can run.
The default is `Unfenced`; existing drivers need no changes. Dynamic `Host`
factories returning `Fenced` are explicitly rejected before connection.

Clients attach exactly one MQTT v5 `target-generation` user property to Desired,
target Config and mutating commands, including empty retractions. A token is
opaque, case-sensitive, and contains 1–128 visible ASCII bytes (`0x21..0x7e`).
Targetless mutations use the instance token. Unknown targets and absent or stale
tokens are refused. Reads may omit the token; `assume` is unavailable in fenced
mode. JSON payloads and the existing topic grammar remain unchanged.

Implement the contextual driver methods (`apply_with_context`,
`execute_with_context`, `configure_with_context`, `read_state_with_context`) and
use the corresponding Reporter methods for state, events and removals. Carry the
original context through queued work and observations. Legacy observation methods
have absent context and cannot update a fenced model. State, events, request
errors and replies retain their original token on MQTT, including sealed batches
and reconnect replay. Never assign the current token to an old observation.

When changing mappings, freeze service admission and serialize hardware work,
durably save the new inventory, then call
`Reporter::replace_generations(&expected_table, next_table)`. It compares the
complete expected table, installs the next authority and retires exact old
runtime work/history under the same lock order used by admission. Report the
complete inventory before making the new service mapping ready. An uncertain
save or failed replacement must keep service admission frozen. Never reuse a
retired token for the same target.

If a momentary MQTT command commits that very change, capture
`reporter.command_completion()` inside its contextual execute future and forward
the opaque capability to the inventory owner. After durability, use
`replace_generations_for_command(&old_table, next_table, &completion)` so that
exact active command can return its final reply with its original token and
correlation data. The capability is single-use, checked atomically with CAS,
and unavailable to reads, validation or spawned helpers. All queued and unrelated
retired work is still refused; stop/disconnect can still cut off the command.
HTTP/other administrative edits use the ordinary replacement method. See the
[command completion contract](../docs/superpowers/specs/2026-10-21-bridge-command-completion.md).

Use the pure `validate_request` hook to reject work during service transitions or
enforce read-only metadata. It runs before cached Config acceptance and outage
conflict decisions, under the operation watchdog. It cannot perform I/O. The
service must also validate the original context atomically with physical address
resolution at dispatch; runtime checks cannot undo an operation already started.
See the [generation contract](../docs/superpowers/specs/2026-10-21-bridge-target-generations.md)
and the tutorial's optional mutable-inventory section.

## The testing kit

A bridge's tests run the real runtime around the real driver, against a fake broker and a stand-in
device, on `127.0.0.1`. **Never use a real broker.**

- **`FakeBroker`** ([`mqtt-test-broker`](../mqtt-test-broker)): an MQTT v5 (or v3.1.1) broker for one
  client at a time, in your test process. It plays the Store: `publish` as another client would (with
  `SendOptions` for retain, message expiry, user properties, response topic, correlation data),
  `retain`, and reading back everything the bridge published (`received_on`, `events`, `retained_on`,
  `subscriptions`). It also injects the faults a bridge must survive: `drop_connection`,
  `restart_without_persistence`, `refuse_connections`, `hold_acks` / `delay_acks` (a broker that does
  not acknowledge), `stop_reading` (a wedged connection), `hold_subacks`, `hold_subacks_matching` (one
  filter's SUBACK alone), `deny_subscriptions`. `FakeBroker::start_multi()` accepts several clients at
  once (two `Host` processes on one root, each with its own will).
- **`Bridge::start(driver)`** runs the bridge in-process on the test's tokio runtime (no signals, no
  logging start, no panic hook) and returns a `Running`; `stop()` is the bounded shutdown. Set the
  broker, instance, timing and `device_stable` in code, with short timings so an outage plays out in a
  second. **`Host::start()`** does the same for a multi-device host (`.broker`, `.name`, `.timing`,
  `.factory_timeout`); the test publishes the retained `Config/{instance}` the factory builds from.
- **The stand-in device** is yours: a small server for your device's protocol. The tutorial's is
  [`examples/tutorial_bridge.rs`](examples/tutorial_bridge.rs) (`tests` module); mqtt_ac's is
  [`tests/support/coolmaster.rs`](https://github.com/yuvalrakavy/mqtt_ac/blob/265df22/tests/support/coolmaster.rs).
- **Negative controls.** For every guard, undo it and see its test fail for the right reason. The
  tutorial does it in code (a `Tamper` driver run through the same scenarios); the runtime does it
  with a table, [`docs/controls/2026-10-04-bridge-runtime-controls.toml`](../docs/controls/2026-10-04-bridge-runtime-controls.toml).

**The conformance suite** ([`mqtt-bridge-conformance`](../mqtt-bridge-conformance)) is the runtime's own
executable contract: a black-box suite that plays the Store's side over `FakeBroker` against a
*scriptable fake device* (itself a `DeviceDriver`: up and down, values changed at the device, scripted
answers per operation — a lost link, a refusal, an unusable reply, a call that never returns, a panic,
a thread-blocking call). Its scenarios: reconnects (a CONNACK's subscriptions before `Active`, a broker
restart without persistence); the fixed instance as the host's N = 1 mode (no packet id reused while
a SUBACK is held, a late read-back, no root subscriptions); requests (held while the device is down and applied within their expiry,
never after; the outage conflict both ways; a target-level write reaching `apply` once; retractions);
commands and replies; driver errors (paced reconnects, one outage episode, unacknowledged writes held
again); malformed payloads; a stalled broker; and the process (`fake-bridge` as a real binary:
SIGTERM, including during a stalled connect). The `host_*` scenarios hold the multi-device host to its
spec's §7 with a scriptable fake factory: routing and independence, Config change, disable, delete
and handover with their output flushed, the per-instance bootstrap and the `Active` barrier, a stuck
SUBACK, a factory that is slow or never returns, two processes on one root, healing, and fairness
under a flood. Every publish is checked against the lanes' JSON
Schemas, retain flags and QoS when a harness stops. It is what changes to the runtime are held to;
**a bridge's author does not run it against their driver**, they test the driver as above. (To run it:
`cargo test --manifest-path mqtt-bridge-conformance/Cargo.toml` from the repository root. It needs
`MQTT_BRIDGE_TEST_SEAMS=1`, which the repository's `.cargo/config.toml` sets.)

`wait-lint` and `lock-order` are the fleet's no-hang tooling the runtime itself is built with; a bridge
may use them too (see the guarantees, below, and mqtt_ac's `tests/wait_registry.rs`).

## The no-hang guarantees

The reason the runtime exists: the bridges it replaces each hung in some copy of this machinery. What
it guarantees to a bridge (details and arguments in the spec's §5.2 and
[`docs/wait-registry.md`](docs/wait-registry.md)):

- **A device call never hangs the bridge.** Every driver call is bounded (connect, operation, probe,
  disconnect) and its future dropped at the deadline; an overrun is a lost link. A synchronous library
  goes through `ctx.blocking`, whose overrun abandons its thread, and whose threads are budgeted (four
  for operations, sixteen for helpers).
- **A driver that blocks its thread is detected and ended.** Its own timers cannot fire, so a
  watchdog on the MQTT side reads the operation's deadline (dead past it and 2 s), the worker's
  heartbeat when idle (dead if silent for 5 s: a helper blocking), and the blocking budgets (dead if
  full for twice the operation bound). It then says the device is unreachable, refuses what waits,
  does the bounded shutdown and exits with a failure for the service manager to restart. A thread
  cannot be cancelled; the restart is the recovery. A pause of the whole process (SIGSTOP) is not
  taken for a dead worker.
- **A stalled or dead broker never blocks the device side.** Reports never wait: `State` coalesces
  per target; `Event` and `Error` go to queues capped at 64 (the oldest displaced, once per episode
  logged); answers to requests are uncapped. The pump only polls; a connection that makes no progress
  for twice the keep-alive (an unacknowledging broker, one that stopped reading) is ended and replaced
  by a fresh client, paced.
- **A device that is down never blocks the session.** The mailbox is posted to and never waited on:
  state requests are held (latest per key) to their deadline, momentary commands are refused at once,
  reads dropped.
- **Outages are episodes, not log floods.** INFO at the first failure, DEBUG for retries, one WARN past
  the threshold, INFO on recovery with `down_for_ms` and `attempts`; reconnects are paced (never a hot
  loop), and recovery is declared on proof of life (a good read or probe), not on a connect.
- **A bad answer is not an outage.** `Rejected`, `Unusable` and `Unsupported` never reconnect.
- **Start and stop are bounded.** The stop signals are taken before anything else; the logging start
  (which touches the file system and DNS) runs on a blocking thread raced against the stop; the
  driver's `info()` too. The shutdown answers every command still waiting, publishes `Active=false`,
  waits for the broker's acknowledgements and sends DISCONNECT, within a 2 s flush, inside a 5 s
  shutdown (with a subscription unacknowledged, a bridge or a host publishes only what its packet
  ids allow; what still waits past the flush's ack wait is not published, and the last will says
  `Active=false`; the model of a device not yet announced on the connection, its read-back still
  reading, is not published at all); the runtime is shut down within 1 s more, never dropped (dropping waits on blocking
  threads without limit). `main` returns an `ExitCode`.
- **Nothing blocks on stdout or stderr.** The runtime writes nothing there on a lifecycle path; a
  panic report goes through one bounded, lossy queue to one thread, waited for at most 200 ms, so a
  stderr nobody drains cannot hold a panicking thread. A panic in the driver or the MQTT task is a dead
  part: ERROR `worker_died`, the bounded shutdown, a failure exit.
- **Logging is lossy.** Destinations never block, and a logging that fails to start does not stop the
  bridge.
- **Every wait is argued.** Each `.await` on something else carries a `// WAIT:` tag naming a row of
  [`docs/wait-registry.md`](docs/wait-registry.md), which says why it cannot hang (`acyclic` or
  `bounded`); `tests/wait_registry.rs` fails the build on an untagged one. Locks come from the
  `lock-order` crate, never raw.

## Building and testing

The repository's tooling crates are packages of their own, not workspace members. From the repository
root:

```sh
cargo fmt --manifest-path mqtt-bridge-kit/Cargo.toml -- --check
cargo clippy --manifest-path mqtt-bridge-kit/Cargo.toml --all-targets -- -D warnings
cargo doc --manifest-path mqtt-bridge-kit/Cargo.toml --no-deps
cargo test --manifest-path mqtt-bridge-kit/Cargo.toml        # includes the tutorial example's tests
cargo build --manifest-path mqtt-bridge-kit/Cargo.toml --examples
```

The `test-seams` cargo feature (environment-variable hooks for the conformance suite) compiles only in
this repository's own builds; a bridge that unifies it in fails to compile, by design. Do not enable it.

## Not built yet

- The stdio plugin protocol, `mqtt-bridge-host` and the Python SDK (spec §7, §8, R3).
- Restore for one-way devices (spec §11, "Deferred").

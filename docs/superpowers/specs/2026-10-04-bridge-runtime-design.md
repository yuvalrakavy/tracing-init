# Bridge Runtime (`mqtt-bridge-kit`) — design

**Status:** approved by the owner, 2026-10-04. R1 built; its gate's findings fixed (§5.2 and the
notes through §5.1, §12), 2026-10-04. The multi-device host (`Host`, the multi-device spec
`docs/superpowers/specs/2026-10-10-multi-device-host-design.md`, §3, §4) built on it, 2026-10-10:
the notes at the end say what it changed here.
**Companions (kept in sync, owner rule 2026-10-04):**
- the SDL Bridge Support Kit spec (Store, `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md`),
  which owns the wire contract;
- the Bridge Kit Tutorial;
- the `developing-mqtt-bridges` skill.

**Development extensions (2026-10-21, not yet upstream/released):**
[Managed process services](2026-10-21-managed-process-services.md) adds a resource
owner whose lifetime survives broker/device outages, with one bounded process
shutdown. [Target generations](2026-10-21-bridge-target-generations.md) adds
optional fencing for fixed controllers with mutable physical inventories.
[Command completion](2026-10-21-bridge-command-completion.md) allows one exact
active MQTT command to return its original-context result across its own
successful inventory commit while all other old work retires.
[Request traces](2026-10-21-bridge-request-traces.md) owns optional validated
causal metadata across ingress, worker and publication queues, including
per-caller coalesced replies. RequestContext comparison remains generation-only;
ReplyTo gains a traceparent field and a constructor for hand-built callers.
Legacy drivers retain their default methods and unfenced behavior. The extension
specifications, crate README and tutorial define the additional APIs and tests;
they continue through the same host/session/publication machinery.

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
| `disconnect()` | after a lost link, at shutdown | be quick; best effort; bounded by the operation timeout after a lost link and by a fixed 2 s at shutdown | nothing |
| `read_state(scope)` | after connecting, on the poll interval, for `refresh`, and before resolving outage conflicts | read the device, not a cache | observed state per target |
| `apply(target, values)` | for a request; several values arrive together for an atomic write | apply all values, in the order the device needs | ok, an error class for the whole write, or `Partial`: the properties that failed, each with its class (the rest were applied) |
| `execute(target, command, params)` | for a momentary command | act once; never retry | ok with an optional reply, or an error class |
| `configure(target, config)` | when a `Config` intent arrives or is retracted (`none`) | accept the setup | ok or an error class |
| `probe()` | for drivers with `writes: unacknowledged`: idle and while busy | prove the device is alive (Lutron's `DLMON`) | ok or `LinkError`; defaults to a read. `Unsupported` proves nothing: from an unacknowledged driver it is a contract violation (its writes are still taken as proven) |

### 4.2 The link context

- `ctx.report.state(target, values, assumed?)`: observed or assumed state, from an operation or a
  helper.
- `ctx.report.remove(target)` and `ctx.report.remove_property(target, property)`: a target gone
  from the device (its retained `State` cleared with an empty retained payload). Cleanup retries
  after CONNACK until PUBACK, within a 1,024-entry/128-KiB backlog; overflow drops the oldest
  unconfirmed cleanup retries with a WARN on the next snapshot drain. A refusal PUBACK is
  terminal and rumqttc logs its reason; it is not retried indefinitely. Public report snapshots
  keep their existing shape.
  `remove_property` removes one property of the target.
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
  it starts until it ends — in use or abandoned, a cancelled caller's too — against a budget by
  kind: four for operations' calls, sixteen for helpers' (long-lived reads, a reader per port or
  zone, never fill the operations' cap; re-gate 3 S7). Past a budget a call is refused as a lost
  link; either budget refusing calls, with no place freed, for twice the operation bound is a dead
  worker — the helpers' too, since a reader whose synchronous reads never return leaks a thread at
  each link loss, and once its budget is full every link is lost at its first call, for good
  (re-gate 4 F4). So the helpers' budget can end the process too: a driver that keeps sixteen
  long helper calls running and has one more refused, with none freed for twice the operation
  bound, is restarted — which is the intent: its helpers are stuck, and only the restart frees
  them (re-gate 5: round 3's "helpers never end it" is superseded). A refusal stamped when a place
  is free again leaves no stamp (re-gate 4 minor 3), and a call whose thread cannot start is a
  lost link too (minor 5).
- `ctx.read_back()` (and `LinkHandle::read_back()`): the bridge's start-up read-back of its own
  retained `State` — `ReadBack { complete, targets }`, frozen as the conflict rule reads it. Its
  values are what the previous run last published (its reports and assumptions, as the broker
  retained them), not device evidence; `complete` is `false` while the read-back is pending, and for
  good when the broker refused it or it came late. The two are one snapshot: the read-back is
  frozen and marked done under one lock, and read under it, so `complete` never vouches for a map
  read before the completion (third confirming reviews). At the first `connect` it may not be done yet:
  a driver that needs it (mqtt_ac v2 seeds its known units, and which were `powered:false`) reads
  it again from its handle.
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
- **No restore in R1.** Restoring a one-way device that comes back dark (DMX, DALI) from the
  read-back is deferred: see "Deferred: restore for one-way devices" under §11. The read-back stays,
  for the conflict rule's "before" values.

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
| Logging | tracing-init with lossy destinations; the fleet's kind names (Store `docs/guides/logging-policy.md`); a `Host` adds `device_config_invalid`, `device_config_rejected`, `subscription_refused`, `output_lost_at_session_end` (WARN) and `slot_reasserted` (INFO), and every record under a device carries `instance` and the bridge id |
| Instances (a `Host` only) | `Config/+` and the effective owner; per instance the bootstrap (S1 read-back, S2 target Configs, S3 requests, each SUBACK matched by packet id), construction off to the side (`--factory-timeout`), the stop (admission closed, the worker stopped, output retired, the lanes unsubscribed), the `via` tags, the slot healing and the delete cleanup; `Active` after a root barrier; sealed batches served round-robin, 32 per device per turn (multi-device spec §4) |

Device-link states: `Connecting` → `Proving` (connected, awaiting the first good read or probe) → `Up`,
or `Down` on `LinkError`, a missed deadline or `link_lost`. `Down` retries after the pacing interval.
Catch-up runs on entering `Up`. While `Down`, state requests are held to their deadline, momentary
commands are refused, and reads are dropped.

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

Source contract and caller/test enumeration: [optional request lifetime limit](2026-10-24-request-lifetime-limit.md).

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
    pre-outage values. A replayed value's broker basis is captured when it arrives and kept
    through reads, re-holds and later disconnects, so a disconnect during its conflict read does
    not replace it; and it is pinned per key: a replay of the same retained value in a later
    session (the Store has not retracted it yet) is decided against the basis it was first
    replayed with, not a later snapshot that already holds the change at the device. A live write
    of the key — property-level or target-level — or a retraction of it, or a replay of another
    value, drops the pin (re-gate R2, R12; second final gate 3). What
    an acknowledged write teaches the bridge changes later snapshots only, never a basis already
    captured. The check applies to each value on its own: a live value taken with a
    checked one is applied as it is.
  - **Unproven assumptions:** for a target with assumed feedback, what the runtime assumes from
    an unacknowledged write is published as assumed `State` at once, but the conflict rule's
    current values and snapshots leave it out — they keep the value before it — until a probe or
    read proves that write (a report of the key settles it too). An unproven write held again
    when the link dies is then not satisfied by, nor superseded by, its own assumption, under
    either conflict policy (re-gate R3). A retraction of the key settles its assumption only when
    no write of it is unproven; an unproven one stays unproven, held again if the link dies before
    proof — the Store retracts once it has seen the assumed `State`, before any proof, so a
    retraction that dropped it would leave the re-hold off in the normal confirm flow (re-gate 4
    minor 7). A write held again at a link's loss keeps that provenance (`StateValue::reheld`): a
    retraction arriving after the re-hold — a delayed confirmation — neither drops it nor settles
    its assumption; it is applied on the next link, until a probe or read proves it or a newer
    request replaces it (Codex final gate 5). An identical retained replay of it is that write still:
    the held one is kept, with its obligation; a different value, or a live request — whatever its
    value — replaces it, with its own deadline and policy, and the replaced write's assumption is
    withdrawn at once, before a retraction of the new request could settle it as proven (second
    final gate 2; confirming reviews 1) — in the same step as the replacement, under the
    mailbox's lock, so the worker cannot take the replacement and assume it first (third
    confirming reviews, Codex 2: withdrawn after, it reverted the replacement's own assumption).
    One that leaves without being applied — expired, superseded, failed — withdraws its assumed
    value: the key goes back to what it was before the write, and `State` says so, for the device
    may never have taken it (the second final gate's minor 1).
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
    bridge's own report. So is one on any kit request lane by its shape —
    `{Root}/{Desired|Command|Config}/{instance}…` under any root — where another bridge would take
    it for a request (re-gate R17, re-gate 3 S6). The `Event` lane stays open: replies
    are Event-shaped (`"kind":"reply"`), and the kit spec has them arrive there.
  - A target-level write's property copies are skipped for 60 s.
  - `configure` is called only while connected. Configs are held while down and applied first in the
    catch-up: every held config before any held value, and a target's waiting config — one that
    came during the catch-up too — before its values (re-gate R5); while up, a target's waiting
    config before its values; `Config` is subscribed before `Desired`. A config's `feedback` override takes effect
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
  - `read_back_incomplete` (WARN): the read-back's SUBACK did not come within 5 s, or refused the
    subscription (an ACL; `refused`); the requests were subscribed without it;
  - (`restore_incomplete` and `restore_failed` went with the restore; see §11's deferred note.)

### 5.2 Bounds and repairs from the R1 gate (2026-10-04)

- **MQTT progress:** a request outstanding with nothing acknowledged for twice the keep-alive
  (10 s at the default) ends the session, and so does one poll that has not returned within it.
  The event loop is dropped, never polled again, and the paced retry makes a fresh client. A
  PINGRESP or a publish from the broker is not progress: a broker that answers PINGREQ and
  withholds its acknowledgements fills rumqttc's in-flight window, and rumqttc then takes no
  request while its pings succeed; a broker that stops reading holds rumqttc in its flush,
  inside the poll, where its keep-alive cannot fire. Reconnects are paced by `poll_retry`, and by
  `retry` after every fifth failed connect in a row — a connection dropped before `stable_after`
  (two bridges sharing a client id) counting as a failed one (Claude's re-gate minor 12); a
  connection's age counts from its CONNACK, so a slow one is no time it held (re-gate 4 minor 9).
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
  - a blocking budget — the operations' or the helpers' — has refused calls, with no place freed,
    for twice the operation bound: threads stuck in synchronous calls for good, so every later
    call would be refused. The worker itself still runs: the verdict refuses nothing and says
    nothing of the device — the stop that follows stops the worker, and its flush refuses what
    waits (re-gate 4 minor 8: refused at the verdict, a live worker served a mailbox marked
    disconnected).

  A watchdog tick that comes more than 1 s late is a pause of the whole process (SIGSTOP, a VM's
  pause): the worker was paused too, so for 2 s after it the watchdog judges nothing (re-gate 3 S6).

  Then: ERROR `worker_died`, for a blocked thread `Status=unreachable` and what waits refused —
  the command the worker was running among them, "cut off"; the refusals go to the session in a
  batch of their own, past the capped `Error` and reply queues, as a link loss's do (second final
  gate 1) — and the dead-part path: the bounded
  shutdown (which does not wait for a blocked worker, whether or not the verdict was read; re-gate
  4 minor 4) and a failure exit, so the supervisor restarts the process. A thread cannot be
  cancelled; the restart is the recovery (re-gate R1, R13).

  The deadline stays on the clock while `op()` drops an operation's future that ran out its
  bound, so a driver whose future's synchronous `Drop` takes longer than the 2 s grace — or a
  bounded synchronous call longer than a lowered `--operation-timeout` plus 2 s — ends the process
  instead of losing the link. That is deliberate: such a call blocks the worker's thread either
  way, and only the restart frees it (Claude's re-gate minor 13).
- **Several workers per session (mqtt_ynca's receivers): built as the multi-device host, 2026-10-10.**
  `Shared` is split into a session part (the client, the topics' root and bridge id, `Version`,
  the devices) and a per-device part (topics, mailbox, reports, commands, the `via` tag, the
  span); the session routes each inbound message by its instance to its device. Each device has
  its own worker thread, keeper and watchdog, and its own `Status` (the lane is per instance, and
  a configured instance is its own instance). The watchdog's verdict stays a process outcome
  (owner decision D4 = a: a blocked worker, an exhausted budget or a factory that does not return
  ends the process, R1's dead-worker path); every other device failure is that device's lost
  link. `Bridge::start` keeps its one driver: a `Bridge` is the same session with one fixed
  device, untagged, with R1's subscriptions and wire unchanged. The panic hook stays
  process-global.
- **Test seams:** the environment variables `MQTT_BRIDGE_TEST_*` (a held logging start, refused
  stop signals, a panicking session) are compiled in only with the kit's `test-seams` cargo
  feature, which the conformance suite enables — never in a deployed bridge, debug build or not
  (Claude's re-gate minor 4).
- **Nothing taken is lost at the end:** the batch a session took from the reports, and how far its
  publication got, are the session's, outside the publish a stop cuts short: the flush finishes
  the publish under way — kept, not dropped, since whether rumqttc's channel took its message
  cannot be told from outside — then publishes what the batch has not enqueued with what is
  pending, answers first, nothing enqueued twice (third confirming reviews, Codex 1: the batch
  was taken whole, and a stop while its States backed up behind a slow broker dropped its reply;
  re-gate 3 S6's "taken and cut is lost" is superseded).
- **What a stop answers:** the bounded shutdown first stops the worker — it takes nothing more,
  the stop checked before every take, and what it runs is cut off — and waits for it (2.5 s at
  most: its driver's disconnect and its runtime's end); only then does the session's flush refuse
  every command still waiting for the device, and the one the worker had taken and was cut off
  from answering, which the mailbox keeps as in flight until it is answered (re-gate 3 S3) — on
  `Error` and on its response topic, published directly rather than through the reports' capped
  queues, which a full mailbox (one command executing, 64 waiting) would overflow (Codex final
  gate 2); so they go out ahead of what was queued before them, nothing lost (the second final
  gate's minor 4: documented) — then publish what is pending (`Status`, `Error`, replies; a
  watchdog's verdict included), then `Active=false`; then it waits until every QoS 1 publish the
  session enqueued has been written and acknowledged — a watermark, not the momentary in-flight
  count — before the DISCONNECT, until 750 ms before the flush's bound, or until the pump ends
  (the connection lost: nothing more will be written or acknowledged — a publish rumqttc refuses
  as it writes it, larger than the broker's maximum packet size, fails rumqttc's poll and ends the
  pump too; third confirming reviews, Claude minors 2 and 4); acks that never come do
  not keep the DISCONNECT back past that, it goes anyway, best effort (INFO `bridge_shutdown_cut`;
  the second confirming review's minor 2). After the session's own DISCONNECT — not one rumqttc
  sends on a protocol error (its minor 3) — the pump reads on until the broker closes the
  connection (500 ms at most; past it, INFO `bridge_shutdown_cut`): closed with the broker's acks unread, the socket was reset, and the
  broker dropped what it had not read yet — the last replies, `Active=false`, the DISCONNECT
  (confirming review N1). All within the flush's bound (re-gate R15): the DISCONNECT's write and
  the linger fit in the 750 ms the ack wait leaves (third confirming reviews, Claude minor 3: 1.5 s
  of ack wait and 1 s of linger overran the 2 s bound). The flush runs on any
  connection the broker accepted — the outage episode's stability window has no say in it (second
  confirming review 2: a stop soon after a reconnect skipped every answer) — as the pump says it:
  a CONNACK the pump has read counts though the stop won the session's select over it, and a
  connection the pump has lost does not (third confirming reviews, Claude minor 1). A cut-off command's error says it was cut off while it ran, so the device
  may or may not have done it; a waiting one's, that it was refused before it ran. **Exactly one
  answer per command:** whoever takes the command out of flight answers it — the worker when its
  operation ends (`Mailbox::command_done_then`, which queues the worker's answer under the same
  lock step that ends the flight, so a flush finds either the command or its answer; re-gate 5
  minor 4), else the flush or the watchdog's verdict — so a command a verdict answered "cut off"
  is not answered again when its blocked call returns, and the worker, stopped before the flush,
  cannot answer between the flush's refusals and its publish (re-gate 4 F3, minors 1 and 2). The
  stop watches the watchdog while it waits for the worker: a verdict that the worker's thread is
  blocked, reached before the stop or during its wait, ends the wait at once (re-gate 4 minor 4;
  re-gate 5: a verdict just after a check made the stop wait out its 2.5 s). After a
  full-budget verdict the worker still runs, and nothing is refused until the stop: an embedder
  that reads the verdict late keeps serving meanwhile, with its blocking budget full.
- **Reports before reads, and a read after every batch:** the session's select takes the stop
  first, then what is pending to publish, then the next incoming event — the reads polled first
  would drain a backlog before a publish and displace its replies past the event cap (re-gate 3
  S2). But after a batch of reports, the incoming events that are ready — up to 64 — go before the
  next batch: a steady report stream (a fade, a meter) to a broker slow but steady to acknowledge keeps
  `reported` ready at every pass, and the requests, commands and SUBACKs would otherwise wait for
  good, the acknowledgements keeping the progress bound from firing (Codex final gate 1). The
  read-back's subscription is dropped once the read-back is done — complete, or late — since its
  frozen copy is all the conflict rule reads: kept, it brought each `State` the bridge publishes
  back as an incoming publish, queued ahead of every request. Some still come: what the first
  batch published before the broker took the UNSUBSCRIBE. Read at one per batch, those drained at
  a read a second, and a command behind them waited past its expiry (the second confirming
  review's minor 5, a flake under load); up to 64 per batch, a backlog drains in a pass or two.
  Answers being uncapped, a backlog read first displaces none of them.
- **Answers are never displaced:** every reply to a request (an outbound message with
  a response topic) and every terminal `Error` of one — a refusal (a stop's, a link loss's, a
  watchdog's, an expired read caller's, one on arrival), a failure (`Rejected`, `Unusable`,
  `Unsupported`, each of a `Partial`'s properties, a command's), a supersession — go to an uncapped
  batch of answers; the capped `Error` and `Event` queues keep only unsolicited reports, malformed
  input among them (Codex final gate 2, second final gate 1, confirming review 2). **A reply follows
  the State its command produced; at the stop, answers go first:** in normal operation each batch
  publishes `Status` and `State`, then the answers, then the unsolicited reports, so a `refresh`'s
  reply never reaches the Store before the State it read (the second confirming review's F1); the
  stop's bounded flush publishes the answers first, before what else is pending (confirming
  review 3).
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
- A multi-device bridge is `Host::new(kind).from_cli().device(factory).run()` (or `.start()` in
  process), with the same option methods, `--name` and `--factory-timeout` in place of
  `--instance`; the factory is `Fn(&DeviceConfig) -> Result<D, FactoryError> + Send + Sync`,
  called on a thread of its own for each accepted `Config/{instance}`
  (`FactoryError::Malformed` / `Unusable` answer `Error/{instance}`).
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
- **The multi-device host:** the multi-device spec's §7 scenarios (`host_*` test files), with a
  scriptable fake factory, each under an outer deadline with a progress assertion.
- **Plugins:** a plugin that crashes, hangs or floods its stdout.

Every guard also gets a negative-control row. The suite runs in this repo's CI.

## 10. Repository layout

```text
mqtt-bridge-kit/          the runtime and the Rust trait binding (library)
  src/  topics · wire · pump · session · mailbox · worker · reports · replies · process · plugin
        host (the `Host` builder) · lifecycle (per instance) · devices (worker threads, keepers)
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

### Deferred: restore for one-way devices

R1 shipped a restore for six gate rounds: at the first read-back, the previous run's assumed values
were posted as requests, for a one-way device that comes back dark. Every round's fix to it brought
new defects, and no adopting bridge needs it yet. It is removed from R1 (`DriverInfo::restore`,
`--restore`, and the mailbox's and session's machinery for it), and comes back when mqtt_dmx
migrates, designed with that bridge as its consumer. The hazards the gates found are its design
inputs:

- **When to post.** The restore must wait for every retained `Config` and `Desired` to be read,
  which comes behind the last request subscription's SUBACK; a late read-back's own SUBACK comes
  ahead of theirs (re-gate 4 F1). A restore that waits for the read-back without a bound hangs a
  catch-up (re-gate 3 RG3-4); one run from the worker's loop never runs under a fast poll (RG3-1).
- **Which targets.** The previous run's `"assumed"` flag is not this run's feedback: a target whose
  `Config` now says `observed` was restored over the device's own value (re-gate 4 F2). The
  feedback must be judged when the value is taken, after its config.
- **A read-back cut short.** The `State` subscription goes with its session; read again later, the
  broker's retained `State` of any target this run has published since is this run's, not the
  previous run's — that value is gone from the broker (re-gate 4 F5, re-gate 5 G2).
- **Keys left to requests.** A key a request was posted for is that request's — but the request
  may not take it: superseded on an unknown value (a `DeviceWins` replay to a one-way device),
  expired, failed, or cancelled — and then the key stays dark (re-gate 4 F6, re-gate 5 minor 1).
  Giving it back has its own traps: a mark that outlives a newer request lets the restore replace
  it (re-gate 5 G1); an older failed request can consume the restore a newer waiting request was
  to inherit (Codex final gate 3); a confirmation that arrives while an apply executes can post the
  restore over that apply's success (Codex final gate 4). Ownership must follow the newest request
  through waiting, flight and outcome.
- **Whose failures.** A restored value is no one's request: its failures and expiry are not the
  Store's `Error` (re-gate 4 minor 10).

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
  session, which subscribes again; but past 5 s without the SUBACK (the session then ends, and the
  next connection goes without it), or with a SUBACK that refuses it (an ACL), the requests are
  subscribed without it (WARN `read_back_incomplete`), and a replay that
  meets no "before" values is applied. The read-back's SUBACK is told by its packet id, and only one
  that grants it counts. The read-back is complete at the next SUBACK, behind its retained replay,
  whichever request it acknowledges;
  incomplete, it stays so: the conflict rule's read-back is frozen as it stood when it was first
  done, so a `State` arriving later — a late SUBACK's replay — does not change "before" values in
  mid-process. After a broker-only outage, replayed requests are checked
  against the model as it stood when the connection was lost; the read-back serves only the
  process's first session (and held values until the device's first outage snapshot). A change at
  the device the bridge never saw — made while the bridge was away, between two reads — is still a
  change.
- **The read-back's order rests on the broker.** The requests are subscribed once the read-back's
  SUBACK has come, so a broker that writes a subscription's retained messages right behind its
  SUBACK (Mosquitto does) queues the retained `State` ahead of every retained request. A broker
  that sent retained messages later, on its own schedule, could still interleave them; and past
  5 s without the SUBACK the requests are subscribed anyway, on the next connection (WARN
  `read_back_incomplete`).


### Notes from the documentation pass (2026-10-10)

- `LinkStatus::Disabled` (`disabled`) is part of the grammar but no R1 bridge reports it; the multi-device host publishes it for a configured instance that is disabled.
- `MqttTiming::poll_retry` is settable in code only (no CLI flag); `--connect-timeout 0` is not yet refused like the other zero timings (follow-up).
- §10's `plugin`, `mqtt-bridge-host` and Python SDK are R3 and not built.

### Notes from building the multi-device host (2026-10-10)

What R2 changed in the R1 runtime, and the points the multi-device spec left open that the build
decided (each one for the owner to confirm or overturn):

- **Unchanged for a `Bridge`:** its API, its wire (no `Config/{instance}`, no `via`), its
  subscriptions and their order, its logging. R1's conformance suite passes unchanged.
- **The pump forwards `UNSUBSCRIBE` and `UNSUBACK`** (with their packet ids), as it forwarded
  `SUBSCRIBE` and `SUBACK`; `NoSubscriptionExisted` counts as a success.
- **Packet ids.** rumqttc 0.25 draws subscribe, unsubscribe and publish packet ids from one counter
  that wraps at the broker's receive maximum, and can hand out the id of a SUBSCRIBE still awaiting
  its SUBACK. MQTT §2.2.1 forbids that, so (R2 gate, Codex 3; the FIFO-per-id workaround removed) a
  host hands rumqttc a SUBSCRIBE or UNSUBSCRIBE only with no publish unacknowledged, and while any
  is outstanding hands out fewer ids after the oldest than the receive maximum (the pump reads it
  from the CONNACK; the publication is budgeted to match). Ids in use are unique, and each
  acknowledgement is matched by its id. A `Bridge`'s fixed CONNACK duties are unchanged (R1): its
  back-to-back subscribes and first publishes could in principle wrap the same way on a broker
  with a small receive maximum — not addressed here.
- **The `Active` gate's "all grants"** excludes the root barrier's own subscription (it is the
  barrier, not a grant the barrier waits for).
- **The bootstrap is sequential:** S2 is requested on S1's grant, S3 on S2's; on a bootstrap after
  the first (a reconnect), S2's retraction of stale target Configs happens at S3's grant.
- **An unowned instance with malformed host fields** (its `via` readable, naming this process)
  is failed (`Status=unreachable`); a **kind mismatch** releases an owned instance *without* the
  delete's cleanup (R2 gate, Codex minor: builder gap 4 overturned).
- **A failed instance** (no driver serves it nor is ready to — a construction under way, a retry's
  included, is no service — and its construction failed or its Config is malformed with none
  accepted) is retried at the device's `--retry` (at least 1 s); a retry of the same failing Config
  logs at DEBUG; one `Error/{instance}` per failing Config. A factory panic is `Unusable`. A Config
  committed and then changed again keeps its built driver until the newer one's construction
  succeeds; if that fails, the committed one serves (R2 gate, Codex 4 / Claude 4: builder gap 12
  overturned). A driver panic, or a worker that ends unasked, ends the process — R1's dead-worker
  path; the multi-device spec's D4 says so (the owner accepted it, R2 gate).
- **A `Bridge`'s stop sender after a blocked verdict** (builder gap 15, accepted): the keeper drops
  the worker's stop sender on the watchdog's verdict, so a worker that was only pending, not truly
  blocked, stops and disconnects before `Running::stop`, where R1 kept it running until the stop.
  Observable only after a death, which ends the process anyway; R1's control row
  `a_driver_call_is_not_bounded` now notices its guard through a timing assertion. Dropping a
  `Running` unstopped closes admission and stops every worker, as R1's did (R2 gate, Codex 8).
- **Re-enabling** a disabled instance seeds its read-back from the instance's retained slots the
  session saw.
- **Untagged non-empty writes** on an owned slot are foreign and reasserted (`slot_reasserted`).
- **A disabled or stopping instance ignores property `Desired`** — the owner's decision
  (2026-10-10), now in the multi-device spec §4.4: it stays retained, replays on re-enable or to a
  successor, and expires on its own deadline; a refusal would repeat at every replay and never
  settle, and `Status=disabled` already says why. Commands and target-level `Desired` are refused
  with a reply.
- **An overdue grant** (no SUBACK within 10 s) logs `subscription_refused` with `overdue_ms`.
- **A `Host`'s defaults:** the root is the kind, the bridge id is the kind, the `Version` text is
  `<kind> <version>`, and with no version set `<kind> (mqtt-bridge-kit <its version>)`.
- **Cleanups survive a session loss:** a delete finished while disconnected, or one whose
  retractions the broker had not acknowledged when the connection went, keeps its cleanup pending
  and, after the next root barrier, sweeps the instance (a short read-back) and retracts only what
  still carries its own tag (R2 gate, Codex 7, Claude minor 2). At its first root replay a process
  also reads `Status/+` and sweeps every instance whose Status carries its tag but which it does not
  own (Claude 3: a delete made while it was down). Answers and unsolicited reports of a device that
  is retired go to the session's outbox, a member of the publication rotation; an outbox dropped
  with its connection is said (WARN `output_lost_at_session_end`).
- **Known minors, not fixed:** there is no periodic healing backstop beyond the per-message one;
  the broker-lost snapshot ignores the read-back (inherited from R1). (The retire-step flood is
  fixed: R2 gate, Codex 2.)

### Notes from the one-runtime merge (2026-10-10)

- **One runtime** (owner decision, the multi-device spec's D9 and §4.9): a `Bridge` is a thin facade
  over `Host` with one fixed instance, named from `--instance`. R1's own session path — its CONNACK
  duties, its blocking read-back wait, its direct republish, its unbudgeted flush — is deleted; the
  host's lifecycle carries the fixed instance's CONNACK duties in R1's order (the read-back alone and
  once per process, its SUBACK awaited within `READ_BACK_WAIT`, then the four request subscriptions,
  then `Active=true`, the `Version` and the model), and R1's conformance suite passes unchanged.
- **Packet ids:** every SUBSCRIBE, UNSUBSCRIBE and publish of a fixed bridge now goes through the
  host's packet-id budget (MQTT §2.2.1), which closes a risk R1 had: `Active`, `Version` and the
  model went out right behind the subscriptions, and with a small receive maximum took the id of a
  SUBSCRIBE still awaiting its SUBACK. As every request, a SUBSCRIBE of a fixed bridge is bounded by
  the host's 10 s grant bound; R1 never waited for its requests' SUBACKs at all.
- **The read-back's UNSUBSCRIBE** waits, as every request does, for the line to clear of publishes
  (nothing published meanwhile), so it may go out ahead of the model rather than behind it.
- **A late read-back ends the session** (the R2 confirming reviews 2, Claude minor 2): past
  `READ_BACK_WAIT` without its SUBACK, the read-back is incomplete (WARN `read_back_incomplete`) and
  the session ends there, rather than going on as R1 did. The abandoned SUBSCRIBE's packet id stays
  in use until its SUBACK, and only a new connection frees it: going on, the budget held the
  publication behind it until the 10 s grant bound ended the session anyway. The next connection
  subscribes the requests at once, without the read-back.
- **The read-back completes at the first request SUBACK** behind its replay, whichever request it
  acknowledges, granted or refused, as R1's did (the R2 confirming reviews 2, Codex 1).
- **The shutdown's flush skips the model of a slot not announced** on the connection (the fixed
  instance's before `Active=true`, a configured instance's before its read-back's grant): published,
  it would overwrite the retained State its read-back is still reading. R1's flush published it (the
  R2 confirming reviews 2, Claude minor 3); its answers and unsolicited reports still go.

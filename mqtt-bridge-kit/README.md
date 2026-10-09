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

- **Learn it:** [`docs/tutorial-writing-a-bridge.md`](docs/tutorial-writing-a-bridge.md) writes a
  complete bridge step by step, with tests, negative controls and a systemd unit. Its code is
  [`examples/tutorial_bridge.rs`](examples/tutorial_bridge.rs).
- **Look it up:** `cargo doc --no-deps --open` (every public item is documented), and this page.
- **The source of truth:**
  [`docs/superpowers/specs/2026-10-04-bridge-runtime-design.md`](../docs/superpowers/specs/2026-10-04-bridge-runtime-design.md)
  (the runtime), and, for the wire contract, the Store's Bridge Kit spec
  (`docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md`) and contract guide
  (`docs/guides/mqtt-bridge-contract.md`) in the Store repository.
- **A real bridge on it:** [mqtt_ac v2](https://github.com/yuvalrakavy/mqtt_ac/tree/265df22) (the
  CoolMaster air-conditioning controller).

The crate is not published (`publish = false`); bridges depend on it by git revision and pin one.
Minimum Rust version 1.80.

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
| the command line, `--help`, usage errors | the driver's own options (`Bridge::option`) |

The contract between the two is the `DeviceDriver` trait: every operation has a meaning, a deadline
the runtime enforces, and an error class that says what the runtime does next.

| Error class | Means | The runtime then |
|---|---|---|
| `Link` | the connection failed: I/O, timeout, closed | drops the link, starts or continues the outage episode, holds state requests, refuses momentary ones, reconnects at the paced rate |
| `Rejected` | the device refused this request | publishes `Error` (target, property); WARN `command_rejected`; the link stays up |
| `Unusable` | an answer the driver could not parse | publishes `Error` once and passes the request over; never reconnects for it |
| `Unsupported` | no such command or property | publishes `Error`; no retry |
| `Partial` | (`apply` only) these properties failed; every other value was applied | publishes `Error` for each failed property alone; the link stays up |

## Topology: one process per controller

An `{instance}` in the topic grammar is *one bridge process, or a gateway it fronts*. The runtime
supports one shape today and plans another:

- **A controller-fronted bridge: one process per controller. This is what this crate builds.** The
  device you talk to is a controller or gateway (a CoolMaster, an HWI, an AV receiver); the devices
  behind it are its **targets** (an indoor unit `L7.400`, a lamp zone). `--instance` names the
  controller, one process serves one device link, and the runtime's one device worker and one driver
  are that link. Several controllers are several processes, each with its own instance. mqtt_ac is
  the worked example: one CoolMaster controller, its indoor units as targets.
- **A device-direct bridge, many devices per process: the planned multi-device host. Not built.**
  Here each device is its own network endpoint (an IR blaster per room, or mqtt_ynca's receivers,
  which the spec names), and one process should host many of them with one MQTT session. Today that
  means one process per device. The runtime's spec records what would have to become per device (its
  §5.2, "Several workers per session"): the mailbox, reports and commands behind a router from target to worker; a `Status` lane
  per device (the grammar has only one per instance); the watchdog's verdict, so that one stuck device
  does not end the whole bridge; and `Bridge::start`, which takes one driver. The read-back and the
  broker snapshot can stay shared. Until that host exists, do not build a many-devices driver on this
  API; run one bridge per device.

## The topic grammar and lanes

The wire contract is the Store's Bridge Kit; the runtime implements the bridge side. Read it in the
Store repository: the spec `docs/superpowers/specs/2026-10-04-sdl-bridge-kit-design.md`, §3 (grammar
and lanes) and §4 (the `Desired` lane, conflicts), and the guide `docs/guides/mqtt-bridge-contract.md`.
This is the summary; [`topics`](src/topics.rs) builds every topic.

```text
{Root}/{Lane}/{instance}[/{target}[/{property}]]
```

`Root` (the device family; `--root`, default the family given to `Bridge::new`), `instance`
(`--instance`), a target and a property are each **one topic segment**: never empty, never containing
`/`, `+` or `#`. QoS 1 everywhere.

| Lane | Direction | Retained | Payload | Meaning |
|---|---|---|---|---|
| `Active/{instance}` | bridge to Store | yes; last will `false` | plain text `true` / `false` | the bridge process is alive and connected |
| `Status/{instance}` | bridge to Store | yes | plain text `connecting` / `connected` / `unreachable` | the device link (the Store grammar also has `disabled`; the runtime does not report it) |
| `Version/{instance}` | bridge to Store | yes | plain text | the bridge's version (`Bridge::version`) |
| `State/{instance}/{target}` | bridge to Store | yes (an empty payload retracts) | JSON object | observed state; `"assumed": true` when not read back |
| `Event/{instance}` | bridge to Store | no | JSON `{kind, target?, ...}` | something happened; **replies** arrive here (`"kind":"reply"`) |
| `Error/{instance}` | bridge to Store | no | JSON `{error, reason, target?, property?, command?}` | a request refused or failed |
| `Desired/{instance}/{target}/{property}` | Store to bridge | yes (empty retracts) | JSON scalar | a requested value the device has not confirmed |
| `Desired/{instance}/{target}` | Store to bridge | never | JSON `{property: value, ...}` | several properties, atomically (one `apply`) |
| `Command/{instance}` | Store to bridge | never; 30 s expiry | JSON `{target?, command, ...}` | momentary actions; the standard `refresh` and `assume`; the driver's own |
| `Config/{instance}/{target}` | Store to bridge | yes (empty retracts) | JSON | per-target setup (to `configure`); may override the feedback mode |

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
`Assumed`), the `commands` (each `Momentary`, `Read` or `WithReply`) and the `Timing` defaults. The
`LinkContext` given to `connect` has the `Reporter` (`state`, `event`, `remove`, `remove_property`),
the helper spawners (`spawn`, `spawn_transient`), `link_lost`, `blocking` (a synchronous call held to
the deadline), `read_back` (the previous run's retained `State`) and `handle()`, a cloneable
`LinkHandle` to keep for later operations. `Bridge` is the builder `main` uses; `Running` is a bridge
started in-process (`Bridge::start`), for tests and embedders.

## The testing kit

A bridge's tests run the real runtime around the real driver, against a fake broker and a stand-in
device, on `127.0.0.1`. **Never use a real broker.**

- **`FakeBroker`** ([`mqtt-test-broker`](../mqtt-test-broker)): an MQTT v5 (or v3.1.1) broker for one
  client at a time, in your test process. It plays the Store: `publish` as another client would (with
  `SendOptions` for retain, message expiry, user properties, response topic, correlation data),
  `retain`, and reading back everything the bridge published (`received_on`, `events`, `retained_on`,
  `subscriptions`). It also injects the faults a bridge must survive: `drop_connection`,
  `restart_without_persistence`, `refuse_connections`, `hold_acks` / `delay_acks` (a broker that does
  not acknowledge), `stop_reading` (a wedged connection), `hold_subacks`, `deny_subscriptions`.
- **`Bridge::start(driver)`** runs the bridge in-process on the test's tokio runtime (no signals, no
  logging start, no panic hook) and returns a `Running`; `stop()` is the bounded shutdown. Set the
  broker, instance, timing and `device_stable` in code, with short timings so an outage plays out in a
  second.
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
restart without persistence); requests (held while the device is down and applied within their expiry,
never after; the outage conflict both ways; a target-level write reaching `apply` once; retractions);
commands and replies; driver errors (paced reconnects, one outage episode, unacknowledged writes held
again); malformed payloads; a stalled broker; and the process (`fake-bridge` as a real binary:
SIGTERM, including during a stalled connect). Every publish is checked against the lanes' JSON
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
  shutdown; the runtime is shut down within 1 s more, never dropped (dropping waits on blocking
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
- The multi-device host (above).
- Restore for one-way devices (spec §11, "Deferred").

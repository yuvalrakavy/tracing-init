# The wait registry

Every place the Bridge Runtime waits on something else — a lock, a channel, the broker, the device
driver, a dependency's `async fn` — carries a tag naming a row here, on a line of its own directly
above the statement:

```text
// WAIT: mqtt-pump-queue
let event = self.rx.recv().await;
```

The row says, once, why that wait cannot hang a bridge: **`acyclic`** (nothing it waits on can wait
back on any of its waiters) or **`bounded`** (it ends within a stated bound, and the row says what
happens on expiry). The rule and the tag format are Store's no-hang spec, §13.3 and §14; the check
is the `wait-lint` crate, run by `tests/wait_registry.rs`. Every std lock is built through the
`lock-order` crate's wrapper with its row's key as its class (`raw-locks = forbid`).

It does not prove the order locks are taken in, nor see a wait reached through a call to this
crate's own `async fn`; the arguments are checked by reading, and the waiters block below makes
every new waiting function a reviewed diff. Regenerate it after a change, from the repository root:

```text
cargo run --manifest-path wait-lint/Cargo.toml -- --root mqtt-bridge-kit --src src --registry docs/wait-registry.md --write
```

**The graph, in one paragraph.** `Bridge::run` runs `serve` on an explicit runtime and then shuts
the runtime down within a bound. `serve` takes the stop signals first, starts the logging on a
blocking thread raced against them, then starts the bridge and waits for a stop signal or the death
of one of its parts, and its bounded shutdown. The pump (`Pump::run`) polls rumqttc's event loop, one
connection per event loop, each poll bounded, and waits on nothing else in this process: its one
send is to an unbounded queue. The session reads that
queue and `Reports`, posts requests to the mailbox — a post never waits — and publishes, waiting on
rumqttc's request channel, which the pump drains. The device worker runs on a thread of its own
with a single-threaded runtime: it takes from the mailbox only while the device is up, calls the
driver one operation at a time under a deadline, and hands what it learns to `Reports`, which never
waits. Every edge leads toward the pump or the driver; the pump waits on nothing here, and every
driver call is bounded — no cycle. Nothing waits on the worker, so a device that is down holds up
nothing but its own requests, and a broker that is down or stalled holds up nothing on the device's
side.

## The rows

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `mqtt-poll` | bounded | the broker, over the network | The pump waits on nothing in this process: it has no client, and its one send is to an unbounded queue. Its wait is on the broker alone, bounded at `stall_bound`, twice the keep-alive (10 s at the default 5 s): one poll may take no longer — rumqttc writes and flushes inside `poll` with no timeout of its own, so a broker that stops reading would hold it until the kernel gives the connection up, and keep-alive cannot fire meanwhile — and requests may stay outstanding no longer with nothing acknowledged, which covers a broker that answers PINGREQ but withholds its acknowledgements. A poll returns at least once a keep-alive while connected (its ping); a connect is bounded by rumqttc's connection timeout (5 s). On expiry, or a failed poll, the pump ends the session: the event loop is dropped, never polled again, a request waiting on its channel fails, the outage episode is advanced, and the MQTT worker makes a fresh client after its pause (`session-retry`). |
| `mqtt-pump-queue` | acyclic | the pump's forward queue | Unbounded (no-hang §14.6), with a high-water WARN (`mqtt_backlog_high`): the pump never waits on the session, so the session's wait ends with the next event, or with `Ended` when the connection has failed five times running — the session returns and a new one starts. If the pump task is gone the queue closes and the wait returns `None`. Its depth and high-water flag are atomics: nobody waits on them, nothing is logged under a lock. |
| `mqtt-request` | acyclic | rumqttc's request channel (`publish`, `subscribe`, `subscribe_many`, `disconnect`) | The channel drains only while the event loop is polled, and the pump polls it in a task of its own. So a full channel is back-pressure on the session, never a cycle: with the broker stalled the session waits until it recovers, the pump reading its acks meanwhile; nothing on the device's side waits behind it (`reports` never waits). The CONNACK duties are the session's, not the pump's. When the pump ends, the event loop is dropped and a waiting request fails; what it was publishing is still in the model, for the next session's CONNACK. Each connection has a client of its own (the pump ends the session on the first failed poll), so nothing handed to one channel goes out on a later connection, and rumqttc's packet-id collision cannot outlive its connection. |
| `session-events` | acyclic | the pump's queue, `Reports`, the shutdown (`select!`) | An idle wait for work: the pump never waits on the session (`mqtt-pump-queue`), a report never waits (`reports-ready`), the shutdown is a watch channel the stop sets. |
| `session-work` | acyclic | one event's handling, raced against the shutdown (`select!`) | The handling waits only on the request channel (`mqtt-request`); the race lets a stop through while the broker stalls a publish, so the shutdown never waits behind it. |
| `session-retry` | acyclic | the pause between MQTT sessions, raced against the shutdown | A timer (`poll_retry`, and `retry` after every fifth failed connect in a row) and a watch channel: nothing here waits on the session. |
| `shutdown-flush` | bounded | `Active=false`, DISCONNECT, and the pump's word that it was written | `FLUSH_BOUND`, 2 s. On expiry one INFO `bridge_shutdown_cut`, and the last will says `Active=false` when the broker notices the connection gone. Skipped at once when the broker is out of reach. |
| `reports-ready` | acyclic | the next report (`Reports::reported`) | An idle wait for work, the session's. A report never waits — a short lock, then `Notify::notify_one`, which keeps a permit when nobody is waiting — so no reporter waits on the session. The retained model coalesces per target; `Error` and `Event` are capped (64 each), the oldest displaced past it, one WARN per episode. |
| `reports-lock` | acyclic | the reports' state (`lock-order` mutex, class `reports-lock`) | Held only to change or copy what waits for the session: never across an await, no other lock taken under it, nothing logged under it — the displacement lines are logged after the guard is dropped. |
| `mailbox-posted` | acyclic | the mailbox, read by the worker (`Mailbox::take`) | An idle wait for work. A post never waits — a short lock, then a notify — so no poster waits on the worker. The worker takes only while the device is up; while it is down nothing takes: state waits (the latest per key, so bounded), momentary commands are refused at once, reads dropped. |
| `mailbox-lock` | acyclic | the mailbox's state (`lock-order` mutex, class `mailbox-lock`) | Held only to read or change what waits: never across an await, no other lock taken under it, nothing logged under it — a refusal or a drop at a deadline is logged after the guard is dropped. The conflict rule's resolver runs under it on the worker's own data, taking no lock. |
| `device-op` | bounded | one driver operation (`connect`, `read_state`, `apply`, `execute`, `configure`, `probe`), raced against the link's `link_lost` | The operation's timing: `connect` for a connect, `probe_deadline` for a probe, `operation` for the rest (driver defaults, each overridable on the command line). On expiry, or when a helper reports the link lost, the operation's future is dropped and the link is lost: helpers cancelled, `disconnect` (bounded), what the device may not have taken held again, momentary commands refused, the outage episode advanced, and the next connect paced. Never a hang. A driver that blocks its thread synchronously holds only the worker's own thread (`worker-thread`), where these timers cannot fire either; the deadline is also on the worker's `OpClock`, which the watchdog on the MQTT side reads (`bridge-parts`): past it and `BLOCKED_GRACE` (2 s) the device worker is dead. |
| `link-lost` | acyclic | a helper's word that the link died (`LinkHandle::lost`) | Only ever raced: against an operation (`device-op`) or the worker's idle wait (`worker-up`). A `link_lost` never waits. |
| `worker-up` | acyclic | the link lost, a probe or poll falling due, the mailbox (`select!`) | The worker's idle wait while the device is up: timers, a notify (`link-lost`), and the mailbox (`mailbox-posted`) — nothing it waits on waits on it. |
| `device-disconnect` | bounded | the driver's `disconnect` | The operation timing after a lost link; `DISCONNECT_BOUND`, 2 s, at shutdown. On expiry the call is dropped: `disconnect` is best effort. |
| `blocking-call` | bounded | a synchronous call on a thread of its own (`LinkHandle::blocking`) | The current operation's deadline. On expiry the thread is abandoned (it cannot be cancelled) and the link is lost. Every call's thread holds a slot under `ABANDONED_CAP` (4) from before it starts until it ends — in use or abandoned, cancelled callers' and helpers' calls included — so at most four run at once, and past that `blocking` refuses at once. With no operation running (a helper's call) it waits for the call, as the helper chose; the helper is cancelled with its link. |
| `worker-thread` | acyclic | the worker's own runtime (`block_on` on the worker's thread) | It runs the worker until the stop, then the bounded disconnect. Nothing waits on this thread: its end is a oneshot the bridge selects on (`bridge-parts`) and, at shutdown, waits for within `SHUTDOWN_GRACE`. |
| `worker-stop` | acyclic | the worker's life, raced against the stop (`select!`) | The stop is a watch channel; the worker waits on nothing that waits on it. |
| `bridge-parts` | acyclic | the MQTT task's end, or the worker thread's (`Running::ended`); and the watchdog's tick | Neither waits on the caller; none ends on its own, so the wait ends only with a death — or is dropped when a stop comes first. The watchdog is a timer that reads the worker's operation deadline (an atomic): past it and `BLOCKED_GRACE` it says the device unreachable, refuses the momentary commands waiting (the mailbox's lock, `mailbox-lock`, held by nobody across a wait) and ends the wait with the device worker's death. |
| `bridge-end` | acyclic | a stop signal, or a part's end (`select!`) | Neither waits on the caller (`stop-signal`, `bridge-parts`); the first ends the wait, and the bounded shutdown follows. A part's end is one ERROR `worker_died` naming it and a failure status. |
| `shutdown` | bounded | the bridge's parts stopping (`Running::stop`, `stop_within`) | `SHUTDOWN_GRACE`, 5 s: the MQTT side's flush (itself bounded at 2 s) and the worker's disconnect (2 s). On expiry one WARN `shutdown_timeout`, the MQTT tasks aborted, the worker thread abandoned; `run` then shuts the runtime down within its own bound (`runtime`). |
| `runtime` | bounded | `serve`, on the runtime (`Runtime::block_on`); then the runtime's threads (`Runtime::shutdown_timeout`) | `serve` ends with a stop signal or a death and the bounded shutdown (`bridge-end`, `shutdown`), or at once on a stop during the logging start (`logging-start`). Then `RUNTIME_GRACE`, 1 s; on expiry the threads still in a synchronous call — a DNS lookup of the broker's name, tracing-init's start — are abandoned and the process exits (the fleet's F1). |
| `logging-start` | bounded | tracing-init's start, on a blocking thread | It reads its configuration, opens today's log file and resolves the GELF host, synchronously; tracing-init bounds the open and the lookup at 5 s each; on expiry it starts without that destination (WARN `log_destination_skipped`) and abandons the thread stuck in it. Raced against the stop signals: a stop ends the wait at once, and the thread is abandoned within `runtime`'s grace (F1, F3). Without the signals (they could not be taken, and the bridge will not start) it waits on the start alone, within `SHUTDOWN_GRACE` (5 s), only to log why; on expiry the bridge ends with a failure status and nothing logged. The logging's own failure is written to stderr by a thread of its own, never joined, so a stderr nobody drains holds neither the start nor the stop. In debug builds only, the start first reads the file `MQTT_BRIDGE_TEST_LOGGING_GATE` names, if set — the test seam that holds the start for as long as a test likes. |
| `panic-report` | bounded | the panic hook's report reaching stderr, written on a thread of its own (`std::sync::mpsc` `recv_timeout`) | `PANIC_REPORT_BOUND`, 200 ms, on the panicking thread. On expiry the writer thread is abandoned — a stderr nobody drains holds it, never the panicking thread — and the panic goes on to its `catch_unwind` or task end. The ERROR `panic` before it goes through tracing, which is lossy and never waits. |
| `stop-signal` | acyclic | the operating system's SIGINT or SIGTERM | Nothing in this process is waited on. Both are registered first thing in `serve`, before the logging starts (F3). |

## Settings

```wait-lint
# rumqttc's client calls, which wait on its request channel, and its event loop's poll.
wait-methods = publish, publish_with_properties, subscribe, subscribe_many, unsubscribe, disconnect, poll
# Dependency calls whose .await waits on nothing in this process.
not-waits = sleep, sleep_until, yield_now
# The driver trait's methods, awaited on a driver: this code's own (every call is bounded by its
# caller, `op`).
local-methods = read_state, configure, probe, blocking
raw-locks = forbid
```

## Waiters (generated)

```wait-lint-waiters
blocking-call src/link.rs LinkHandle::blocking
bridge-end src/process.rs serve
bridge-parts src/process.rs Running::ended
device-disconnect src/worker.rs Worker::down
device-disconnect src/worker.rs spawn
device-op src/worker.rs Worker::check
device-op src/worker.rs Worker::configure
device-op src/worker.rs Worker::link
device-op src/worker.rs Worker::probe
device-op src/worker.rs Worker::prove
device-op src/worker.rs Worker::serve
device-op src/worker.rs Worker::work
device-op src/worker.rs op
link-lost src/link.rs LinkHandle::lost
logging-start src/process.rs Bridge::serve
mailbox-lock src/mailbox.rs Mailbox::locked
mailbox-posted src/mailbox.rs Mailbox::take
mqtt-poll src/pump.rs Pump::run
mqtt-pump-queue src/pump.rs Incoming::recv
mqtt-pump-queue src/session.rs flush
mqtt-request src/session.rs connected
mqtt-request src/session.rs emit
mqtt-request src/session.rs flush
panic-report src/process.rs panic_hook
reports-lock src/reports.rs Reports::locked
reports-ready src/reports.rs Reports::reported
runtime src/process.rs Bridge::run
session-events src/session.rs session
session-retry src/session.rs run
session-work src/session.rs session
shutdown src/process.rs Running::stop
shutdown src/process.rs serve
shutdown src/process.rs stop_within
shutdown-flush src/session.rs flush
stop-signal src/process.rs StopSignals::next
worker-stop src/worker.rs spawn
worker-thread src/worker.rs spawn
worker-up src/worker.rs Worker::serve
worker-up src/worker.rs sleep_until
```

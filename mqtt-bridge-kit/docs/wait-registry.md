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

**The graph, in one paragraph.** `Bridge::run` (and `Host::run`) runs `serve` on an explicit
runtime and then shuts the runtime down within a bound. `serve` takes the stop signals first, starts the logging on a
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
side. A multi-device host has one session and a worker per device, each with its keeper: the keeper
waits on its worker's end, its watchdog's verdict and a stop request — none of which waits on the
keeper — and a factory runs on a thread of its own, its watcher bounded by `--factory-timeout`. The
session never waits on a keeper or a factory: their outcomes are events on an unbounded channel it
selects on, and a death goes to a channel `Running::ended` reads.

## The rows

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `mqtt-poll` | bounded | the broker, over the network | The pump waits on nothing in this process: it has no client, and its one send is to an unbounded queue. Its wait is on the broker alone, bounded at `stall_bound`, twice the keep-alive (10 s at the default 5 s): one poll may take no longer — rumqttc writes and flushes inside `poll` with no timeout of its own, so a broker that stops reading would hold it until the kernel gives the connection up, and keep-alive cannot fire meanwhile — and requests may stay outstanding no longer with nothing acknowledged, which covers a broker that answers PINGREQ but withholds its acknowledgements. A poll returns at least once a keep-alive while connected (its ping); a connect is bounded by rumqttc's connection timeout (5 s). On expiry, or a failed poll, the pump ends the session: the event loop is dropped, never polled again, a request waiting on its channel fails, the outage episode is advanced, and the MQTT worker makes a fresh client after its pause (`session-retry`). |
| `mqtt-pump-queue` | acyclic | the pump's forward queue | Unbounded (no-hang §14.6), with a high-water WARN (`mqtt_backlog_high`): the pump never waits on the session, so the session's wait ends with the next event, or with `Ended` when the connection has failed five times running — the session returns and a new one starts. If the pump task is gone the queue closes and the wait returns `None`. The flush's ack wait waits on the pump's progress signal (`Incoming::progress`, told after every event the pump takes, and at its end — however it ends — so the wait also ends when the pump does) the same way, and under the `shutdown-flush` bound. Its depth and high-water flag are atomics: nobody waits on them, nothing is logged under a lock. |
| `mqtt-request` | acyclic | rumqttc's request channel (`publish`, `subscribe`, `subscribe_many`, `unsubscribe`, `disconnect`) | The channel drains only while the event loop is polled, and the pump polls it in a task of its own. So a full channel is back-pressure on the session, never a cycle: with the broker stalled the session waits until it recovers, the pump reading its acks meanwhile; nothing on the device's side waits behind it (`reports` never waits). The CONNACK duties are the session's, not the pump's. When the pump ends, the event loop is dropped and a waiting request fails; what it was publishing is still in the model, for the next session's CONNACK. A batch's publish (`Batch::publish`) that the stop cuts short is kept, not dropped — whether the channel took its message cannot be told from outside — and the flush finishes it first, under its own bound (`shutdown-flush`). Each connection has a client of its own (the pump ends the session on the first failed poll), so nothing handed to one channel goes out on a later connection, and rumqttc's packet-id collision cannot outlive its connection. The lifecycle (`Lifecycle::drive`, which hands rumqttc its SUBSCRIBEs and UNSUBSCRIBEs as their packet ids allow — a fixed bridge's as a host's: a `Bridge` is the host's N = 1 mode) and the publication rotation's turn (`Outputs::turn`) wait on the same channel, from the session's own task: the same back-pressure; a subscription's acknowledgement is not waited for here — it comes as a pump event, its packet id matched, bounded by `GRANT_BOUND` (`session-events`). |
| `session-events` | acyclic | the pump's queue, every device's `Reports` (one shared signal), a host's lifecycle events, its timers, the shutdown (`select!`) | An idle wait for work: the pump never waits on the session (`mqtt-pump-queue`), a report never waits (`reports-ready`), a keeper or a construction's watcher sends its event on an unbounded channel and goes on (`device-keeper`, `device-factory`), a timer is a timer — the grant bound (`GRANT_BOUND`, 10 s: an overdue SUBACK or UNSUBACK ends the session), a fixed bridge's read-back wait (`READ_BACK_WAIT`, 5 s: on expiry its requests are subscribed without it, WARN `read_back_incomplete`), a host's replay fallback (5 s), a failed instance's retry, the sweep — and the shutdown is a watch channel the stop sets. |
| `session-work` | acyclic | one event's handling, raced against the shutdown (`select!`) | The handling waits only on the request channel (`mqtt-request`); the race lets a stop through while the broker stalls a publish, so the shutdown never waits behind it. |
| `session-retry` | acyclic | the pause between MQTT sessions, raced against the shutdown and a host's lifecycle events | A timer (`poll_retry`, and `retry` after every fifth failed connect in a row), a watch channel and an unbounded channel: nothing here waits on the session. A lifecycle event taken meanwhile is handled with no client (`Lifecycle::on_event_offline`): nothing is subscribed, nothing waits, and the pause goes on. |
| `shutdown-flush` | bounded | `Active=false`, every publish of the session written and acknowledged, DISCONNECT, and the pump's word that it was written | `FLUSH_BOUND`, 2 s; the acknowledgement wait ends 750 ms before it (INFO `bridge_shutdown_cut`), or when the pump ends, and the DISCONNECT goes anyway, best effort; the DISCONNECT's write and the pump's linger after it (`LINGER_BOUND`, 500 ms) fit in what is left. On expiry one INFO `bridge_shutdown_cut`, and the last will says `Active=false` when the broker notices the connection gone. Skipped at once when the broker is out of reach. A bridge or host with a SUBSCRIBE or UNSUBSCRIBE outstanding publishes only the ids left before its packet id would come round, and waits on the pump's queue for an acknowledgement for the rest, until the same ack deadline; past it nothing more is published and no DISCONNECT is sent (INFO `bridge_shutdown_cut`): the last will says `Active=false`. |
| `reports-ready` | acyclic | the next report (`Reports::reported`) | An idle wait for work, the session's. A report never waits — a short lock, then `Notify::notify_one`, which keeps a permit when nobody is waiting — so no reporter waits on the session. The retained model coalesces per target; `Error` and `Event` are capped (64 each), the oldest displaced past it, one WARN per episode. |
| `reports-lock` | acyclic | the reports' state (`lock-order` mutex, class `reports-lock`) | Held only to change or copy what waits for the session: never across an await, no other lock taken under it, nothing logged under it — the displacement lines are logged after the guard is dropped. |
| `mailbox-posted` | acyclic | the mailbox, read by the worker (`Mailbox::take`) | An idle wait for work. A post never waits — a short lock, then a notify — so no poster waits on the worker. The worker takes only while the device is up; while it is down nothing takes: state waits (the latest per key, so bounded), momentary commands are refused at once, reads dropped. |
| `mailbox-policy` | acyclic | complete mapping replacement notification | A replacement never awaits the worker. A persistent Notify permit wakes its connected loop or its outage retry loop to discard retired caches. Retrying retains the original absolute reconnect deadline. No report lock is held while waiting. |
| `worker-retry` | bounded | the original device retry deadline or a policy-change notification | Mapping changes may wake the loop to clear caches but never extend the deadline; on expiry it retries at the original instant. The notifier never waits on the worker. Process shutdown cancels the worker. |
| `mailbox-lock` | acyclic | the mailbox's state (`lock-order` mutex, class `mailbox-lock`) | Held only to read or change what waits: never across an await, nothing logged under it — a refusal or a drop at a deadline is logged after the guard is dropped. The one lock taken under it is the reports' (`reports-lock`), which takes none: the worker's answer queued as its command leaves flight (`Mailbox::command_done_then`), and a replaced re-held write's assumption withdrawn as the post replaces it (`Mailbox::post_state_then`, `post_target_then`). Nothing takes the mailbox's lock under the reports', so the order is one way. The conflict rule's resolver runs under it on the worker's own data, taking no lock. |
| `device-op` | bounded | one driver operation (`connect`, `read_state`, `apply`, `execute`, `configure`, `probe`), raced against the link's `link_lost` | The operation's timing: `connect` for a connect, `probe_deadline` for a probe, `operation` for the rest (driver defaults, each overridable on the command line). On expiry, or when a helper reports the link lost, the operation's future is dropped and the link is lost: helpers cancelled, `disconnect` (bounded), what the device may not have taken held again, momentary commands refused, the outage episode advanced, and the next connect paced. Never a hang. A driver that blocks its thread synchronously holds only the worker's own thread (`worker-thread`), where these timers cannot fire either; the deadline is also on the worker's `OpClock`, which the watchdog on the MQTT side reads (`bridge-parts`): past it and `BLOCKED_GRACE` (2 s) the device worker is dead. |
| `link-helpers` | acyclic | one link's helper list (`lock-order` mutex, class `link-helpers`) | Held only to push a helper's abort handle or take the list: never across an await, no other lock taken under it, nothing logged under it. Taken on the device worker's thread, by a helper's start and by the link's drop. |
| `link-lost` | acyclic | a helper's word that the link died (`LinkHandle::lost`) | Only ever raced: against an operation (`device-op`) or the worker's idle wait (`worker-up`). A `link_lost` never waits. |
| `worker-up` | acyclic | the link lost, a probe or poll falling due, the mailbox (`select!`) | The worker's idle wait while the device is up: timers, a notify (`link-lost`), and the mailbox (`mailbox-posted`) — nothing it waits on waits on it. |
| `device-disconnect` | bounded | the driver's `disconnect` | The operation timing after a lost link; `DISCONNECT_BOUND`, 2 s, at shutdown. On expiry the call is dropped: `disconnect` is best effort. |
| `blocking-call` | bounded | a synchronous call on a thread of its own (`LinkHandle::blocking`) | The current operation's deadline. On expiry the thread is abandoned (it cannot be cancelled) and the link is lost. Every call's thread holds a slot under `ABANDONED_CAP` (4) from before it starts until it ends — in use or abandoned, cancelled callers' and helpers' calls included — so at most four run at once, and past that `blocking` refuses at once. With no operation running (a helper's call) it waits for the call, as the helper chose; the helper is cancelled with its link. |
| `worker-thread` | acyclic | the worker's own runtime (`block_on` on the worker's thread) | It runs the worker until the stop, then the bounded disconnect. Nothing waits on this thread: its end is a oneshot the bridge selects on (`bridge-parts`) and, at shutdown, waits for within `SHUTDOWN_GRACE`. |
| `worker-stop` | acyclic | the worker's life, raced against the stop (`select!`) | The stop is a watch channel; the worker waits on nothing that waits on it. |
| `bridge-parts` | acyclic | the MQTT task's end, or a death on the devices' channel (`Running::ended`) | None waits on the caller; none ends on its own, so the wait ends only with a death — or is dropped when a stop comes first. Each device's keeper sends its death — its watchdog's verdict, its worker's end nobody asked for — and a construction's watcher its factory's overrun; each is kept on the unbounded channel until read. The watchdog (`supervise`) is a task of its own per device, a timer that reads atomics only — the worker's operation deadline, its runtime's heartbeat (a timer on the worker's thread, which a blocked thread stops), the blocking calls' cap — and on a verdict says the device unreachable and refuses the momentary commands waiting (the mailbox's lock, `mailbox-lock`, held by nobody across a wait); its verdict is the task's end, which its keeper reads. The keeper aborts it when it ends. |
| `bridge-end` | acyclic | a stop signal, or a part's end (`select!`) | Neither waits on the caller (`stop-signal`, `bridge-parts`); the first ends the wait, and the bounded shutdown follows. A part's end is one ERROR `worker_died` naming it and a failure status. |
| `shutdown` | bounded | the bridge's parts stopping (`Running::stop`, `stop_within`) | `SHUTDOWN_GRACE`, 5 s: every device's keeper asked to stop its worker and waited for at most `WORKER_STOP_BOUND` (2.5 s; a keeper ends at once on a verdict that its worker's thread is blocked), the MQTT side's flush (itself bounded at 2 s), then what is left of the keepers. On expiry one WARN `shutdown_timeout`, the MQTT tasks aborted, the worker threads abandoned; `run` then shuts the runtime down within its own bound (`runtime`). |
| `runtime` | bounded | `serve`, on the runtime (`Runtime::block_on`); then the runtime's threads (`Runtime::shutdown_timeout`) | `serve` ends with a stop signal or a death and the bounded shutdown (`bridge-end`, `shutdown`), or at once on a stop during the logging start (`logging-start`). Then `RUNTIME_GRACE`, 1 s; on expiry the threads still in a synchronous call — a DNS lookup of the broker's name, tracing-init's start — are abandoned and the process exits (the fleet's F1). |
| `logging-start` | bounded | tracing-init's start, on a blocking thread | It reads its configuration, opens today's log file and resolves the GELF host, synchronously; tracing-init bounds the open and the lookup at 5 s each; on expiry it starts without that destination (WARN `log_destination_skipped`) and abandons the thread stuck in it. Raced against the stop signals: a stop ends the wait at once, and the thread is abandoned within `runtime`'s grace (F1, F3). Without the signals (they could not be taken, and the bridge will not start) it waits on the start alone, within `SHUTDOWN_GRACE` (5 s), only to log why; on expiry the bridge ends with a failure status and nothing logged. The logging's own failure is written to stderr by a thread of its own, never joined, so a stderr nobody drains holds neither the start nor the stop. With the `test-seams` feature only (the conformance suite), the start first reads the file `MQTT_BRIDGE_TEST_LOGGING_GATE` names, if set — the test seam that holds the start for as long as a test likes. |
| `driver-info` | acyclic | the driver's `info()`, on a blocking thread | The thread runs only the driver's `info()`, which takes nothing of this process's and so cannot wait on `serve`; it should be pure and return at once, and one that blocks breaks the driver contract. Raced against the stop signals: a stop ends the wait at once — the bridge exits 0, never having started — and the thread is abandoned within `runtime`'s grace (Codex, `process.rs:251`). Without a stop it is waited for: no bridge can start without its driver's description. |
| `panic-report` | bounded | the panic hook's report reaching stderr, written by the process's one reporter thread (`std::sync::mpsc` `recv_timeout`); and that thread's own wait for the next report | `PANIC_REPORT_BOUND`, 200 ms, on the panicking thread, which hands its report over with `try_send` to a queue of 16 (full, the report is dropped). On expiry the panic goes on to its `catch_unwind` or task end; a stderr nobody drains holds the one reporter thread, never a panicking thread, and never a second one (re-gate R8). The reporter waits for reports and writes stderr; nothing waits on it but the bounded handover. The ERROR `panic` before it goes through tracing, which is lossy and never waits. |
| `stop-signal` | acyclic | the operating system's SIGINT or SIGTERM | Nothing in this process is waited on. Both are registered first thing in `serve`, before the logging starts (F3). |
| `session-devices` | acyclic | the broker-loss list, every device's reports (`lock-order` mutex, class `session-devices`) | Held only to push, remove or copy the list: never across an await, no other lock taken under it, nothing logged under it. The pump's loss callback copies the list out, releases it, and only then snapshots each device's model (`reports-lock`). |
| `devices-registry` | acyclic | the running devices' keepers (`lock-order` mutex, class `devices-registry`) | Held only to insert, look up, remove or close: never across an await, no other lock taken under it, nothing logged under it. A stop request under it is a watch channel's send, which never waits. |
| `device-keeper` | bounded | a device's worker end, its watchdog's verdict, a stop request, the stop's deadline (the keeper's `select!`); and the process's stop waiting for each keeper's end (`all_ended`) | With no stop asked, an idle wait for a death: the worker's end and the watchdog's verdict never wait on the keeper. Once a stop is asked, `DEVICE_STOP_BOUND` (10 s): the worker's cooperative stop (its disconnect within 2 s), or the watchdog's verdict that its thread is blocked (within its heartbeat's 5 s), comes first; on expiry the device is dead — a death on the channel, and the process ends (D4 = a). The process's stop waits for the keepers within `WORKER_STOP_BOUND` and `SHUTDOWN_GRACE` (`shutdown`). |
| `device-factory` | bounded | a construction's progress, on its watcher's channel: the factory's return, then the driver's `info()` | Each within `--factory-timeout` (10 s by default); the factory and `info()` run on a thread of their own, which cannot be cancelled. On expiry a death on the channel — D4 = a: the process ends, for the supervisor's restart frees the thread. The thread sends and goes on; nothing waits on the watcher. |
| `managed-lifecycle` | acyclic | the process service's phase (`lock-order` mutex) | Serializes readiness, stopping, fatal/completion facts, worker admission and the earliest shutdown deadline. Never awaits, logs, calls driver methods or drops service resources under it. Admission constructs the host's internal state and starts threads/tasks with already validated info; this orders before the internal registry lock, which never takes the managed lock. Watch sends publish immutable facts; receivers release watch borrows before taking this lock. |
| `managed-start` | bounded | validated readiness from the service worker | `ServiceTiming.startup` (10 s default). `ready` computes pure driver info on the isolated worker, checks expiry/cancellation, seeds heartbeat and transfers once. On expiry or worker exit, cancels the service and waits for teardown only within the shared 5 s stop ceiling. A dropped startup future forces cancellation through its guard. |
| `managed-thread` | acyclic | the service's isolated current-thread runtime | Nothing joins this OS thread. Startup, heartbeat and completion are observed independently by the parent. After cancellation the parent waits only within `managed-stop`; a synchronously blocked worker or destructor is abandoned. Completion is sent after service, LocalSet and runtime teardown. |
| `managed-run` | acyclic | the service future or forced cancellation | The parent writes cancellation without waiting. Cooperative shutdown is `managed-cancel`; after its configured grace the force branch drops the service future and LocalSet. Synchronous blocks are detected by `managed-watch`; the parent remains bounded independently. |
| `managed-cancel` | acyclic | cooperative stop watch | Only the service waits; the parent sets it without waiting. It is also set on startup cancellation, fatal death and owner drop. Service authors must not await MQTT disconnection during teardown. |
| `managed-watch` | bounded | worker completion or heartbeat timer | Reuses the device watchdog's operation/idle bounds and process-pause grace. Heartbeat is seeded before polling service code and again at readiness. On expiry or failure the watcher latches a persistent fatal cause, closes registry admission and cancels the session without requiring `Running::ended` to be polled. |
| `managed-fatal` | acyclic | persistent service fatal watch | Only `Running::ended` waits, raced against signals by the process runner. The independent watcher sets the fatal fact without waiting on its consumer. Repeated reads retain the same cause. |
| `managed-fatal-stop` | bounded | device keepers and session shutdown after service death | Uses the same absolute 5 s component shutdown deadline as explicit stop, even with an unpolled `Running`. Admission closes synchronously first; the session stop is requested after at most the existing 2.5 s worker grace. On expiry the cleanup wait is abandoned; explicit stop reports failure at the original ceiling. |
| `managed-stop` | bounded | actual service teardown completion | Cooperative grace `ServiceTiming.stop` (2 s default), then forced cancellation, all within the earliest absolute 5 s component shutdown deadline. Later cleanup calls cannot extend it. Completion timestamp must precede that ceiling; a cancellation flag alone is not completion. On expiry a blocked worker/destructor returns false; managed process shutdown exits unsuccessfully. |
| `managed-runtime` | acyclic | actual service runtime destruction | After dropping the service future and LocalSet, runtime drop confirms its blocking helpers have ended. This may block only the isolated service thread: nobody joins it. The parent's `managed-stop` deadline independently reports failure/abandonment, and no completion fact is published while a runtime helper or destructor is still running. |
| `process-launch` | acyclic | bridge launch raced against OS stop | Ordinary launch is immediate; managed launch waits on `managed-start`. A signal drops the startup future, retaining `StartupSlot` to confirm service teardown within the original component ceiling. No-ready workers cannot leave live resources behind a successful process exit. |

| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `mqtt-buffer-state` | acyclic | connection epoch, overflow and charged publication map | Short synchronous critical sections. Reports may reserve an answer under its lock; buffering never takes Reports, the mailbox, or recovery locks, and never awaits or invokes callbacks under its lock. Charge destruction changes atomic counters only. |
| `mqtt-buffer-overflow` | acyclic | overflow notification, or disabled pending future | Session selects this independently of publication work and shutdown. Producers latch overflow without waiting on consumers; registering the notification precedes reading the latch. A disabled future is always raced against normal work and shutdown. |
| `recovery-state` | acyclic | recovery phase, pending installation, snapshot publication state | No awaited I/O or engine/driver call under the lock. Installation is taken out before mailbox/Reports replacement. Watch receivers release their borrows before acquiring this lock. |
| `recovery-identity` | acyclic | process-local owner identity counter | Checked increment only; no other locks, callbacks, or waits under the lock. Exhaustion returns an error. |
| `recovery-snapshot-identity` | acyclic | immutable snapshot identity counter | Checked increment only; no other locks, callbacks, or waits under the lock. Exhaustion returns an error. |
| `recovery-install` | acyclic | session installation request/result notification | Engine and session run independently; the session takes installation requests without waiting on the engine. The initial decision has INSTALL_WAIT (10 s); session loss abandons the request and closes its result. Service code races completion with cancellation. Disabled session notification is pending and raced with shutdown. |
| `recovery-readback` | acyclic | session replay watch | Session independently produces a complete replay or ends/retries its connection. The service races readback with cancellation; no MQTT progress depends on a waiting reader. |

## Settings

The recovery state lock is released before model/mailbox installation, awaiting an
engine decision, or publishing to MQTT. Watch borrows are cloned and released before
taking it. The identity locks only increment checked integers. The buffering lock
contains no awaits or callbacks; Reports may take it while reserving answers, but
buffering never takes Reports or the mailbox, so that edge cannot return.

```wait-lint
# rumqttc's client calls, which wait on its request channel, and its event loop's poll.
wait-methods = publish, publish_with_properties, subscribe, subscribe_many, unsubscribe, disconnect, poll, stop_until
# Dependency calls whose .await waits on nothing in this process.
not-waits = sleep, sleep_until, yield_now
# Service runtime destruction may wait for blocking helpers, on its isolated thread only.
wait-fns = destroy_service_runtime
# Managed service deaths are internal.
local-methods = died
# The driver trait's methods, awaited on a driver: this code's own (every call is bounded by its
# caller, `op`).
local-methods = read_state, configure, probe, blocking
# The host lifecycle's handlers and the publication rotation's turn, awaited by the session: this
# code's own; the waits inside them are tagged.
local-methods = on_connack, on_ack, on_event, on_event_offline, on_timer, on_turn_done, on_session_lost, on_publish, on_line, on_recovery, turn
raw-locks = forbid
```

## Waiters (generated)

```wait-lint-waiters
blocking-call src/link.rs LinkHandle::blocking
bridge-end src/process.rs serve
bridge-parts src/process.rs Running::ended
device-disconnect src/worker.rs Worker::down
device-disconnect src/worker.rs spawn
device-factory src/devices.rs Spawner::build
device-keeper src/devices.rs all_ended
device-keeper src/devices.rs keeper
device-keeper src/devices.rs sleep_until
device-op src/completion.rs under_completion
device-op src/driver.rs DeviceDriver::apply_with_context
device-op src/driver.rs DeviceDriver::execute_with_context
device-op src/link.rs under_deadline
device-op src/worker.rs Worker::catch_up
device-op src/worker.rs Worker::configure
device-op src/worker.rs Worker::decide
device-op src/worker.rs Worker::link
device-op src/worker.rs Worker::probe
device-op src/worker.rs Worker::prove
device-op src/worker.rs Worker::serve
device-op src/worker.rs Worker::work_inner
device-op src/worker.rs Worker::write_values
device-op src/worker.rs op
devices-registry src/devices.rs Registry::locked
driver-info src/process.rs serve_with
link-helpers src/link.rs <LinkContext as Drop>::drop
link-helpers src/link.rs spawn_helper
link-lost src/link.rs LinkHandle::lost
logging-start src/process.rs serve_with
mailbox-lock src/mailbox.rs Mailbox::locked
mailbox-policy src/mailbox.rs Mailbox::policy_changed
mailbox-posted src/mailbox.rs Mailbox::take
managed-cancel src/managed.rs ServiceContext::cancelled
managed-fatal src/managed.rs Managed::died
managed-fatal-stop src/managed.rs Control::fatal
managed-lifecycle src/managed.rs Control::complete
managed-lifecycle src/managed.rs Control::deadline
managed-lifecycle src/managed.rs Control::fatal
managed-lifecycle src/managed.rs Control::stopping
managed-lifecycle src/managed.rs Managed::admit
managed-lifecycle src/managed.rs Managed::already_failed
managed-lifecycle src/managed.rs ServiceContext::ready
managed-run src/managed.rs spawn_recovery
managed-runtime src/managed.rs spawn_recovery
managed-start src/managed.rs Bridge::start_managed_in
managed-stop src/managed.rs Bridge::start_managed_in
managed-stop src/managed.rs Control::stop_until
managed-stop src/managed.rs Managed::stop_until
managed-stop src/managed.rs StartupSlot::stop
managed-stop src/process.rs Running::stop
managed-stop src/process.rs serve_with
managed-thread src/managed.rs spawn_recovery
managed-watch src/managed.rs spawn_recovery
managed-watch src/managed.rs watch
mqtt-buffer-overflow src/buffering.rs overflowed
mqtt-buffer-state src/buffering.rs Buffers::begin
mqtt-buffer-state src/buffering.rs Buffers::reserve_reply
mqtt-buffer-state src/buffering.rs Buffers::sent
mqtt-buffer-state src/buffering.rs Connection::acknowledged
mqtt-buffer-state src/buffering.rs Connection::claim_loss
mqtt-buffer-state src/buffering.rs Connection::reason
mqtt-buffer-state src/buffering.rs Connection::reserve_incoming
mqtt-buffer-state src/buffering.rs Connection::written
mqtt-poll src/pump.rs Pump::run
mqtt-poll src/pump.rs linger
mqtt-pump-queue src/pump.rs Incoming::recv
mqtt-pump-queue src/session.rs flush
mqtt-request src/lifecycle.rs Lifecycle::drive
mqtt-request src/session.rs Batch::publish
mqtt-request src/session.rs Outputs::turn
mqtt-request src/session.rs emit
mqtt-request src/session.rs flush
panic-report src/process.rs PanicReports::report
panic-report src/process.rs PanicReports::start
process-launch src/process.rs serve_with
recovery-identity src/recovery.rs RecoveryShared::new
recovery-install src/lifecycle.rs Lifecycle::recovery_changed
recovery-install src/recovery.rs RecoveryHandle::complete
recovery-install src/recovery.rs RecoveryShared::changed
recovery-readback src/recovery.rs RecoveryHandle::readback
recovery-snapshot-identity src/recovery/publication.rs RecoverySnapshot::new
recovery-state src/recovery.rs RecoveryHandle::complete
recovery-state src/recovery.rs RecoveryHandle::receipt
recovery-state src/recovery.rs RecoveryShared::begin
recovery-state src/recovery.rs RecoveryShared::install_deadline
recovery-state src/recovery.rs RecoveryShared::install_pending
recovery-state src/recovery.rs RecoveryShared::lost
recovery-state src/recovery.rs RecoveryShared::readback_result
recovery-state src/recovery.rs RecoveryShared::readiness
recovery-state src/recovery/publication.rs RecoveryShared::offer
recovery-state src/recovery/publication.rs RecoveryShared::publication_ack
recovery-state src/recovery/publication.rs RecoveryShared::publication_deadline
recovery-state src/recovery/publication.rs RecoveryShared::publication_rejected
recovery-state src/recovery/publication.rs RecoveryShared::publication_sent
recovery-state src/recovery/publication.rs RecoveryShared::publication_written
recovery-state src/recovery/publication.rs RecoveryShared::take_publication
reports-lock src/reports.rs Reports::locked
reports-ready src/reports.rs Reports::reported
runtime src/process.rs run_with
session-devices src/session.rs Outputs::add
session-devices src/session.rs Outputs::remove
session-devices src/session.rs Shared::broker_lost
session-events src/session.rs session
session-events src/session.rs sleep_until
session-retry src/session.rs run
session-work src/session.rs session
shutdown src/process.rs Running::stop
shutdown src/process.rs serve
shutdown src/process.rs stop_before
shutdown src/process.rs stop_devices_then_session
shutdown-flush src/session.rs flush
stop-signal src/process.rs StopSignals::next
worker-retry src/worker.rs Worker::run
worker-stop src/worker.rs spawn
worker-thread src/worker.rs spawn
worker-up src/worker.rs Worker::serve
worker-up src/worker.rs sleep_until
```

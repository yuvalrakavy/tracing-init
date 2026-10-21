# Target-scoped state deferral

2026-10-22. T3 continuation of the approved DALI v2 integration. Shared baseline
153d38051f5197b060fa4281efe6136cffee7419; DALI baseline 9976d9b.

## Verified problem and choice

DALI's controller link proves its managed inventory service independently of UART
readiness. This is necessary for metadata and healthy fixtures to remain usable.
The SDK worker abandons Rejected state requests (worker::write_values), whereas
Link errors disconnect the whole instance and retain state for reconnect. The
current SDK has no target-scoped deferral facility. Both source reviewers traced
this boundary. A retained brightness request consumed before HAT readiness, or
during a bus outage, can therefore be lost until another broker replay/request.

Use an explicit driver outcome plus the existing SDK mailbox, with a paced
per-request retry timestamp. The worker stays connected. Existing drivers keep
their current behavior unless they opt into the new outcome. No DALI-owned intent
queue, new MQTT connection, process per fixture or dependency on a Store consumer.

Alternatives: a whole-instance Link outage would block metadata and healthy buses;
a proxy queue would duplicate expiry/retraction/generation/conflict ownership;
sleeping inside apply would block the sole worker and eventually lose its link;
unconditionally retrying Rejected would turn permanent errors into infinite work.

## Outcome and scope

Add OpError::Deferred with a reason and retry_after Duration. It means the driver
has not applied any part of this state intent and a later attempt may succeed.
Supported in state validation, apply, and the read performed to decide pending
state. A failed conflict read can defer without inventing current data. Queued
refreshes and momentary/configuration operations remain one-shot; this outcome
does not cause them to replay. Classify it as an ordinary unavailable/refused
operation there using the existing Error/reply grammar, not whole-link failure.

Proof-only operations are an explicit exception: Deferred from startup proof,
its fallback probe, or an unacknowledged-write probe becomes paced Link failure.
It proves no answer or write acceptance and must never call proven(). This also
prevents a past-due proof timer from spinning. DALI keeps its metadata-only proof
and existing aggregate-read rejection behavior; it does not declare the entire
controller Deferred merely because one physical bus is unavailable.

The worker clamps retry delays to 100 ms..60 s, preventing zero-delay spin or an
unbounded timer. Original MQTT expiry remains authoritative, including while
waiting, during revalidation and immediately before dispatch. No new expiry
window is created. A request without expiry retains its existing no-expiry policy.
Do not report applied/written/assumed success or drop intent on Deferred.
Do not publish a terminal Rejected/Refused Error for a supported state deferral
while retaining that intent: a consumer can retract upon that error. Diagnostics
are non-terminal debug/info. Convergence tests assert no terminal Error before
eventual completion, expiry or retraction.

OpError is a public enum; adding a variant requires exhaustive in-tree adapters
and examples to handle it. Mailbox support structs may gain fields, as the prior
generation-fencing contract already permits. No required driver trait method or
MQTT wire field changes. Verify compiled examples and default unfenced behavior.
The shared revision is unpublished and must be pinned exactly by DALI afterwards.

## Ownership, replacement and scheduling

Keep retry metadata on the existing StateValue: optional captured before-basis
plus next-attempt Instant. Derive/cloning/revalidation retain it with the original
value, context, deadline, conflict policy and reheld/proof obligation. Session
constructs ordinary new requests with no retry metadata. Never restamp generation
or trace when deferring. A genuine newer live request replaces the old key and its
retry metadata. Retraction, expiry, generation retirement, instance disable/delete
and stop use existing mailbox ownership and assumption cleanup.

Under the mailbox lock, defer only the still-in-flight surviving values: require
the exact original authority, no newer pending same-key value, and unexpired
deadline. Clear that in-flight ownership and requeue the surviving originals at
the back. Preserve the first before-basis on repeated deferral, while updating
only the next-attempt timestamp. A same-value/context retained replay must not
erase an existing deferral or extend its original deadline; cover both pending
and in-flight originals. Property-copy suppression continues to run first.

Requeue deferred values as ordinary Entry::State held=false, while preserving
their Check.held/replay history and deferred basis. Each original receives at
most one attempt per catch-up pass. Searching later eligible held entries must
not revisit an ordinary deferred entry even if its timer expires during another
slow target. Otherwise catch-up can remain perpetually busy and never reach
serve, where ordinary commands and reads run.

Mailbox next/take_held/take_group must skip not-yet-due state without parking other
targets, metadata configuration, reads or momentary work. Expired state is removed
even if its retry time is later. Group only eligible state entries for one target,
held class and context, preserving target config-before-state ordering. A deferred
state is not a barrier that delays a momentary command until hardware returns.
Do not change the momentary non-retry rule.

Mailbox::take races its post notification against the earliest retry/expiry timer,
then re-evaluates under the lock. Timer expiry alone authorizes nothing. Keep the
existing notification lost-wakeup safety and cancellation contract: no ownership
is removed before an await that can be canceled. Catch-up must finish when only
future deferred values remain, allowing ordinary serve to handle their timers;
it must still reach other eligible held targets behind a deferred entry. The same
eligibility rule applies after a poll, through try_take. Existing poll/probe/stop
fairness remains intact. No detached retry task or second mailbox.

## Conflict basis and original context

Before any awaited state decision/write attempt, capture the time, known values
and policy atomically under one Reports lock, using a crate-private timed snapshot
helper. A separate Instant::now outside snapshot_with_policy is insufficient:
broker_lost can sample between the values and their timestamp, reversing which
basis is older. The known values include acknowledged written knowledge, excluding
unproven assumptions as defined by Reports. Only the matching mapping may supply
the deferred property's before-value. On the first actual deferral store that
pre-attempt basis; repeated reads/retries must not replace it with recovery state.

The conflict decision treats a deferred value as checked even when it was first
received live. Add its captured source to Befores' existing broker/whole-device
sources, selecting the oldest applicable known value with the existing fallback
rule. Retained replay basis and genuine whole-link outage semantics remain intact.
For Observed feedback, read current target state again before retrying, then revalidate expiry,
replacement, retraction and generation after the await. DeviceWins and RequestWins
retain their existing meaning; a satisfied request need not be written again.
Assumed feedback retains its existing cache-based conflict policy.

Both ordinary Work::Apply and initial catch_up must capture the pre-attempt basis
and recognize Deferred in validation, decision-read, post-read validation and
apply. Deferral in decide must not be followed by write_values(empty) clearing
another request's ownership. Do not run a deferred catch-up target twice using a
stale proving snapshot from before its next attempt. Preserve explicit original
trace absence and each request's captured cause across retry and broker reconnect.

## DALI adoption

Add a typed service Unavailable outcome for pre-execution UART/bus unavailability;
do not classify errors by string. Preserve Busy as a distinct no-admission result.
Permanent validation, disabled bus, retired mapping, unsupported operations and
expired requests remain permanent outcomes. Closed service remains Link failure.
Uncertain operations that may already have transmitted bytes remain explicit
uncertain errors; Deferred must never claim they had no physical effect.

Map typed pre-execution Unavailable and Busy to Deferred only on state paths.
Check physical readiness without acquiring admission when possible, but recheck
in the actual owner before enqueue; an outage racing admission must also be typed
Unavailable if execution did not start. A conflict-state read while UART/bus is
unavailable defers the state intent. Ordinary refresh reports the unavailable
outcome, and the snapshot helper still publishes Available=false/Quality.

Read-only failures are a separate safe boundary from started applies: transport,
session/bus epoch loss, cancellation or stale evidence during a read did not
apply the pending state intent. Preserve typed Unavailable through output::execute,
Hat::complete_output, owner acceptance and the proxy's final snapshot check,
after the required settlement/cleanup. The frontend may also time out after a
read, or resume after a bus transition; neither may turn a still-pending state
decision into a terminal Rejected/unknown conflict result. Keep permanent mapping,
disabled-bus and unsupported validation distinct. Do not classify by message text.

When a target read supplies only partial, MASK or no-response evidence, its raw
sample remains in the owned cache and State remains Available=false with its
actual Quality. The MQTT target read returns Deferred until current usable control
values exist. Thus a pending state decision waits rather than DeviceWins guessing
or abandoning it. An ordinary refresh remains one-shot: it gets an unavailable
error and cached unavailable State, with no automatic refresh replay. Update the
previous refresh-success expectation for missing fixtures explicitly. Observed
target reads do not claim current values from retained last-known scalars.

Use typed acceptance outcomes internally to distinguish transient/stale evidence
from retired mapping. A read resumed after source invalidation defers; a mapping
retired during the await remains permanently refused by exact generation. A
started write's failed final acceptance remains uncertain and never claims safe
deferral. Metadata commands retain their ordinary one-shot error mapping.

Preserve a private execution-started fact through actual cleanup. An admitted
request still Pending behind health work has not run its Output helper. Bus/epoch
loss there is known unstarted unavailability; begin_close must not erase that
fact when it changes the stage to Closing. Keep actual close/permit retirement
before releasing admission. Expiry remains Expired. Any apply request whose Output
helper started remains conservatively uncertain on loss; never infer no effect
from missing readback or a frontend cancellation. Ordinary metadata Writing is
temporary Busy for state paths; uncertain saved routing still requires recovery.

Keep the existing worker-thread readback publication and final snapshot checks.
A retry after recovery publishes actual observed values before acknowledgement,
including clamping. Metadata and a healthy fixture remain usable while another
bus is down. Retained startup Desired waits within its original expiry for HAT
readiness. This slice does not promise automatic retry of already-uncertain
physical writes or invent fixture presence from a HAT N terminal.

## Implementation files and proof

Shared: driver.rs; mailbox.rs plus focused retry scheduling tests; worker.rs plus
actual worker tests; conformance fake-device controls and black-box tests;
compiled tutorial adapters, README/tutorial/runtime specs, wait registry and
developing-mqtt-bridges skill. DALI: typed service/hat pre-execution outcomes,
proxy state error mapping; real managed MQTT/PTY tests; support/wire guides and
phase ledger. Preserve v1 application behavior and config schema.

Failing-first and causal tests:

- Actual DALI bus outage, consumed live Desired, bus recovery without MQTT
  reconnect/reassertion: eventually actual fixture changes; metadata and another
  healthy fixture work while the request waits. Independently inspect emulator
  state and MQTT trace/generation, not only a successful method return.
- Retained Desired subscribed before HAT becomes ready, then readiness without a
  broker reconnect; expire another request before readiness and prove no write.
- Worker validation deferral, conflict-read deferral, post-read validation and
  apply deferral, in serve and catch-up; exactly one eventual admitted apply.
- Future retry at the mailbox front cannot block healthy targets, configuration,
  reads, momentary refusal, later held targets or shutdown. A zero delay cannot
  spin; a future retry cannot hide an earlier expiry or renew total lifetime.
- Two slow held targets whose deferral timers repeatedly elapse cannot keep
  catch-up looping: an ordinary queued command/read must progress before either
  original is retried in serve.
- Deferred from the real default unacknowledged-write probe and startup/fallback
  proof paths never settles unproven writes or advertises false link proof; retry
  is paced and stop remains responsive.
- Newer live value, retraction, replacement during an awaited read, generation
  rename/back/address reuse, disable/stop and same retained replay pending/in-flight
  never resurrect the old value or extend its original deadline/cause.
- DeviceWins detects independently changed recovery state using the first basis;
  RequestWins applies; repeated failed reads and broker reconnect do not overwrite
  that basis. A satisfied request performs no physical write.
- Unchanged clamped readback remains actual knowledge before ack. Uncertain
  possibly-transmitted writes remain errors rather than claiming safe deferral.
- A DALI Pending output behind real health work followed by bus loss/EOF retires
  through actual cleanup and returns typed Unavailable without any intent frame;
  an actually started apply helper under the same loss remains Uncertain.
- A deferred DeviceWins intent passes readiness, loses bus/transport during its
  actual conflict-read await, then recovers without a broker replay: no terminal
  Error for the held state, original TTL/context retained, eventual physical
  convergence. Repeat with partial/MASK/no-response and with a read resumed after
  its service result was invalidated before frontend acceptance. Ordinary refresh
  stays one-shot and still publishes unavailable cached evidence.
- A controlled Reports/broker-loss ordering probe pins the atomic timestamp and
  values/policy snapshot used by oldest-source conflict selection.
- Existing Rejected/Unsupported/Partial and unfenced drivers remain unchanged;
  ordinary refresh/configuration/momentary failures never replay as state.

Pin meaningful guard-removal controls for each new dispatch/scheduler/authority
boundary. No compile-error REDs. Run controls sequentially with no concurrent
source reads/builds, then restored full shared kit/conformance, strict all-target
Clippy, Pi library compilation, compiled examples/wait registry/format checks;
repin DALI and run its native/strict scoped/Pi gates and affected controls.

Two sequential source-grounded design reviews before production edits, then
failing-test-first implementation and two sequential source reviews of a frozen
commit. Same-family models only. No live broker, HAT, Store, push or deployment.

## Parent self-review

Traced mailbox take_group/next/take_held/take/revalidate/disconnected/retraction and
generation retirement, worker serve/catch_up/decide/write_values, and Reports'
known snapshots and before-source selection. A rejected request is abandoned;
watch State after HAT recovery cannot restore that intent. Existing sleeps and
whole-link reconnect do not solve independent target availability.

The first source-based design pass identified cancellation before cursor progress,
same replay during an in-flight retry, original no-expiry requests, catch-up
front-entry starvation, config-before-state ordering and original first-basis
preservation as distinct requirements above. The second pass traced adjacent
failure arms: Deferred from conflict reads/validation, expired deferred work,
momentary/configuration one-shot behavior and uncertain physical writes. Tests
must exercise these paths directly; a generic successful retry cannot stand in
for each dispatch boundary. Memory/skill checklist: ownership remains in the SDK;
both conflict options remain supported; source-to-wire context is explicit;
independent physical emulator evidence is required; documentation/skill changes
ship with the new contract. No Store or panel representation changes are needed.

The first design gate required five amendments, now incorporated: one attempt
per original per catch-up pass; Deferred is never proof of unacknowledged writes;
Pending's unstarted fact survives actual DALI close; basis time/values/policy are
one atomic Reports sample; and supported deferral emits no terminal failure that
would make a consumer retract retained intent. The Observed/Assumed distinction
is explicit. These are required implementation/test boundaries, not optional
follow-ups.

The second design gate found one remaining read-only boundary: an interruption
during the actual conflict-read await still became Rejected/unknown and abandoned
DeviceWins state. The adoption section now separates all read-only transient
loss/late acceptance from possibly transmitted applies, preserves typed outcomes
through cleanup, and defines partial/MASK/no-response and ordinary refresh behavior.
Its causal recovery tests are required. The second reviewer confirmed approval
after this amendment and found no other material design blocker. Both reviews
are source-only and same-family; no implementation tests have run yet.

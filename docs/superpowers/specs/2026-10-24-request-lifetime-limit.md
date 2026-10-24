# Optional request lifetime limit

Status: implementation continuation under the owner's approved DALI architecture
and three-minute command policy. T2: additive public builder API, existing queues.

## Purpose and selected shape

DALI requests that cannot start within a few minutes must expire. Store's DALI
producer already caps RequestExpiry at180 seconds. The runtime currently accepts
requests without an expiry indefinitely (`wire::request_meta`), so other consumers
can leave unbounded DALI work. Existing fleet consumers retain that behavior unless
they opt in. No new publisher timestamp, history, timer or queue is introduced.

Chosen: `Host::request_lifetime_limit(Duration)` and matching `Bridge` facade,
default absent. Store the option privately on Host, copy through session Shared
into each Device, and reuse the mailbox/worker deadlines. Zero is a startup usage
error. Public Settings, DriverInfo, RequestMeta, Changes and Model shapes do not change.
Checked Instant addition refuses an unrepresentable limit rather than panicking.

Alternatives: a driver-only check misses standard commands and pre-driver queues;
a global default changes unrelated bridges' contracts. Both are rejected. The option
applies equally to the fixed-instance and factory-created-device paths.

## Admission contract and enumeration

The origin for every deadline is the original pump receipt Instant, already passed
to session routing. No deadline is recomputed at worker dequeue or reconnect.

| Input | No option | Option enabled |
|---|---|---|
| Nonempty property Desired, finite expiry within limit | Existing deadline | Keep shorter original deadline |
| Nonempty property Desired, no expiry or expiry beyond limit | Existing behavior | Malformed Error; no mailbox mutation |
| Empty property Desired retraction | Existing context-aware retract | Unchanged; no expiry required |
| Nonretained target Desired | Existing expiry or none | min(existing expiry, receipt+limit); missing expiry uses limit |
| Nonretained Command, including refresh/assume/driver reads | Existing expiry or none | Same bounded deadline, preserving replies/correlation/context |
| Retained target Desired or Command | Existing malformed refusal | Unchanged |
| Expiry zero or dispatch after deadline | Existing expiration paths | Existing expiration paths; never reset from dispatch time |
| Config, State readback, output cleanup | Existing behavior | Unchanged |

Property Desired requires a finite bounded wire expiry even on live delivery: MQTT
normally clears RETAIN for an existing subscriber, so that flag cannot prove the
publisher did not retain the packet. Publishers must set the original MQTT expiry
at or below the configured limit. Broker remaining expiry cannot prove a packet's
original publication age; this option does not claim otherwise or add a journal.

Already-started operations retain their existing operation timeouts; the limit is
an admission/queued-start deadline, not a forced abort of a commissioning operation.
The runtime reports malformed policy input through the existing Error lane. Expired
Commands retain the existing refused Error/reply behavior and original request cause.

## Concrete code plan

1. Add regression cases to session tests against the existing route seam, plus
   actual Host/Bridge construction coverage. Record RED with the policy absent.
2. Add private option plumbing in host.rs, session.rs and lifecycle.rs; expose the
   two builder methods in host.rs/process.rs. Centralize bounded metadata/deadline
   calculation on Device; call it from both Desired forms and Command.
3. Test fixed and configured devices through actual builders/FakeBroker. Exercise
   elapsed receipt time, short/zero/missing/long expiry, scalar live/replay refusal,
   empty retractions, target atomicity, builtins and reply metadata. Keep an absent
   policy control demonstrating legacy indefinite/long request behavior.
4. Update runtime design, README, Rustdoc, tutorial, Store kit/contract reference
   and developing-mqtt-bridges skill. Document finite TTL for all DALI property
   examples, including generic consumers. No Store production or persisted changes.
5. After SDK checks/review, publish its feature revision and pin DALI only. Enable
   180 seconds in the canonical daemon. Add a real-daemon/FakeBroker policy test,
   update affected DALI producer fixtures, and run native/production/Pi checks.

## Verification and review

The named seam is session::route plus the private Device option; queued work is
observed through Mailbox and actual driver/report calls. Received timestamps in the
past exercise expiry without sleeping. Public builder integration prevents tests
from proving a private option that never reaches production.

One focused RED/GREEN cycle, then SDK all-target tests and full unfenced conformance
once, strict crate Clippy, and source review of the frozen diff. Only new guards
need negative controls. No Store native/template rebuild is relevant to this slice.
Use the existing warm tracing-init target directory, not the cleaned Store lane.

Self-review: two passes traced the three route arms, both Device constructors,
facade and reply handling. The live-property RETAIN ambiguity was resolved by
requiring wire expiry for every nonempty property Desired. No other mechanism is
needed; one fresh T2 design verification precedes code. Cross-family coverage is
unavailable. Skills: ways-of-working, staged-design-review and developing-mqtt-bridges.


## Design verification disposition

One GPT-6 Astra/high design reader approved without blocking findings. Its two
low guardrails are implemented: Commands retain their existing parsing and ignore
the Desired-only conflict property; zero/unrepresentable limits are refused before
managed resource acquisition, with checked admission addition retained. Coverage
is source-only and same-family.

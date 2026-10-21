# Original trace context through bridge queues

## Intent and observed defect

DALI canonical-daemon acceptance found a shared-runtime defect: an incoming
WithReply command with W3C trace id `112233445566778899aabbccddeeff00` produces a
successful reply bearing a different trace. The test installs a real OTel layer
with a private loopback HTTP collector, under the normal INFO filter. This is
an observed behavior failure, not an inference from absent logs.

Source grounding: session::traced creates a DEBUG span that is disabled at INFO;
Mailbox entries/Work carry no trace; RequestContext carries only generation;
ReplyTo preserves correlation but no trace; session::Out constructs properties
under the later session task and context() stamps only generation. Raising the
ingress span alone cannot preserve a cause across these queues.

Fix the common runtime, not a DALI-only wire workaround. Keep request-generation
authority, admission, completion capabilities, deadlines, conflicts and the
default unfenced wire behavior intact. No new queues, process lifecycle, logging
destination or MQTT property names are introduced.

## Chosen representation and ownership

Extend RequestContext's private storage with bounded optional validated W3C
traceparent metadata. Its equality, ordering and hash remain generation-only:
traces are diagnostic causes, never inventory authority, coalescing keys or a
command-completion identity. Implement/document those comparisons explicitly.
Constructors remain compatible and default to no trace. Provide read access and
an explicit builder for embedding/drivers. Reject/ignore invalid trace metadata
without rejecting an otherwise valid functional request; retain at most 512
bytes and use tracing-init's parser. Duplicate headers are ignored as ambiguous.
Distinguish unspecified constructor metadata from captured absence: wire callers
own either their captured trace or an explicit absence. Captured absence clears
ambient publication metadata; it must not inherit an unrelated session/primary
caller's trace. A constructor's unspecified metadata preserves existing behavior.

Wire parsing captures valid original metadata for fenced and unfenced requests.
When an active OTel ingress span has the same trace id, capture its current
traceparent so later work is parented to the ingress span. Without an explicit
valid header, a valid active ingress context may supply the newly created root.
Never replace a valid incoming trace with an unrelated ambient trace. Generation
parse failures may still produce a traced refusal; telemetry never legitimizes
an invalid generation.

RequestContext already travels through state/config/commands/reads, caches,
retirement reports and sealed output context. Preserve that metadata through
those existing owned clones. No global lookup keyed by topic/correlation is safe
enough to substitute for ownership.

Coalesced read callers can have different traces despite sharing a generation.
Extend public ReplyTo with optional traceparent, populated by from_properties,
and apply each caller's original trace to its own reply/refusal. Add a constructor
so hand-built callers can default to no trace. This is a source migration for
existing test struct-literal locations in kit sources; ordinary
DeviceDriver implementations do not construct ReplyTo. Document the added field
rather than hiding a source compatibility change. Wire shape stays compatible.
ReplyTo captures the same validated ingress cause (including a newly created
ingress root for a headerless request). Its None is authoritative absence, not
permission to use a coalesced caller's cause. Revalidate/bound its public field
when constructing the sealed reply, so hand-built invalid overrides are ignored.

## Enabled carriers and sealed publications

Use an INFO ingress carrier for request lanes Command/Desired/Config only,
created as a new root then reparented to a valid incoming trace before entry.
Keep retained State readback and nonrequest traffic at DEBUG; do not export an
INFO span per state observation. No INFO events are added per message.

At worker dispatch, enter one INFO operation carrier using the owned context,
before validation/driver work/answers. This also covers post-poll dispatch and
held/config work through the same helper. For a combined atomic property batch,
select its latest request as the primary execution cause; individual errors
still retain their own context. Coalesced read execution has one primary cause,
while replies preserve each caller's trace. Polls/unsolicited device work remain
under their existing lifecycle context. No span is made inside a per-property
loop. If OTel is not installed, carriers do not manufacture wire trace metadata.

Frame context sealing replaces the ambient session traceparent with the owned
request/observation trace, exactly once, before enqueue/publication. Later retry,
retirement, disconnect or unrelated work cannot restamp it. Generation/via/
correlation remain unchanged. With no owned trace, preserve existing output
behavior. Driver-provided contextual observations can carry the operation cause
explicitly; do not infer one from a newer inventory generation.
Immediate malformed/refused responses, expired/disconnected/retired callers,
normal replies and already-sealed retries all use the same per-caller rule.

DALI's metadata actor crosses another queue. Its admitted Request already owns
RequestContext; enter an operation carrier around the actor's save/recovery and
carry that original trace when publishing the resulting inventory under the new
generation. Do not reuse the retired generation merely to keep a trace.

## Verification and integration

The queued-read follow-up adds the optional
`read_state_with_request_context(scope, &RequestContext)` driver hook. Refresh
and individual conflict reads pass their original owned cause through it; its
default delegates to the existing contextual read method. Unsolicited reads and
observation generation authority are unchanged. See
[the read-context design](2026-10-22-queued-read-request-context.md).

Keep the failing real canonical-daemon test as acceptance, and add shared tests
for context validation/bounds/duplicates, generation-only Eq/Ord/Hash, original
sealed trace after unrelated work/table retirement, and separate replies for
coalesced callers with distinct trace ids. Verify INFO continuity and no INFO
carrier for retained State; no OTel means no fabricated trace property. Tests
must assert the intended behavior rather than compilation-only changes.

Run the full kit suite, existing unfenced conformance, strict all-target Clippy,
rustdoc and armv7 compilation. Add guard-removal controls, restore then retest.
The bridge pins the resulting reviewed local commit consistently across all
shared dependencies and reruns its full native/daemon/trace tests. Publish no
commit upstream or release during this slice. Update runtime spec, README,
tutorial and canonical bridge skill with ownership, source migration and volume
limits. Two sequential T3 design reviews and fresh source review are required;
cross-family review and physical/live Logmon validation remain unavailable.

## Implementation verification

Implemented on the isolated development branch. Source reviews reproduced and
fixed merged-property attribution, contextual State removals/builtin assume,
dynamic routing and malformed-generation refusals, per-caller read expiry,
and unrelated ambient adoption by public headerless parsers. Their behavioral
regressions were observed RED before the fixes. Two sequential source reviewers
approved; both are the available same model family, with source-only scope.

Restored verification: 211 kit tests and 174 existing conformance tests passed;
strict all-target Clippy for both crates, rustdoc, armv7 library compilation and
diff checks passed. Eight controls in `mqtt-bridge-kit/docs/trace-controls.toml`
failed at their intended assertions, with a recorded ledger. A restored run
exposed a pre-existing test race: releasing the proving read did not mean the
worker was connected before a momentary command. The test now waits for that
actual status; the complete 211-test kit suite and strict Clippy passed again.
This changes only test synchronization, not runtime admission.

README/tutorial/runtime design and the canonical bridge skill were synchronized.
The DALI daemon acceptance and local OTLP actor integration remain downstream
verification, not part of these shared gate counts. No upstream publication,
deployment, physical-device or live Logmon validation is implied.

# Request context for queued driver reads

2026-10-22. Narrow shared-SDK prerequisite for the approved DALI v2 MQTT
integration. Continue the T3 migration with backward-compatible API defaults.
The SDK checkout is tracing-init-dali-v2, feat/dali-managed-services, baseline
96a03bcb9f932d8d49d479a38cc95f9ad9e08ff6.

## Verified problem and choice

DeviceDriver::read_state_with_context(scope) returns contextual observations but
does not receive the original request. Worker::work owns that context for a
queued refresh (worker.rs:910), and Worker::decide owns one before its individual
outage-conflict read (worker.rs:976). Both currently call the contextless input
API. INFO operation spans preserve a trace only when an OTel layer exists; they
cannot carry the original valid MQTT header across another service queue when
OTel is absent. The DALI service API already accepts and preserves owned context.

Keep the existing read method and add an optional
read_state_with_request_context(scope, &RequestContext) returning the same
Vec<ContextualState>. Its default delegates to read_state_with_context(scope),
which already delegates to an ordinary legacy read_state implementation.
Existing drivers keep source/behavior compatibility. No mailbox, wire schema,
generation policy or separate read scheduler is introduced.

Alternatives: replacing the existing signature would break all drivers; reading
the current tracing span would still lose explicit causes without OTel; a global
lookup would duplicate queue ownership and could confuse coalesced requests.
The optional default method preserves the actual source context through the
existing call boundary.

## Dispatch and authority

Queued refresh calls the new method with its Work::Read context, including a
valid original generation and trace or authoritative trace absence. Coalescing
and the primary context selection stay unchanged; each reply still uses its own
ReplyTo context. No expired caller is newly admitted by this change.

The individual conflict read calls the new method with the context already
selected by decide for that target. Preserve the existing first surviving
value selection rather than changing conflict or diagnostic policy here.
Generation revalidation before and after the await stays intact.

Startup proof, periodic poll and default probe keep their existing read paths:
they are unsolicited work without an original request. Drivers implementing
the new hook may preserve the supplied cause across their own queue. They must
still capture the observation's actual mapping token during resolution. The
SDK default never relabels returned observations with a current/request token.
Trace metadata is diagnostic, never authority; no global ambient lookup or
fabricated cause is added. The existing runtime operation deadline wraps the
new call exactly as it wrapped the old one.

## Implementation and proof

Touch driver.rs and the two worker dispatch sites; add focused worker tests and
a real FakeBroker refresh test. Update README, tutorial and the original trace
spec/controls, plus the bridge-development skill's contextual-read guidance.
Do not change tracing destinations, runtime lifecycle or DALI properties yet.

First reproduce loss behavior with an existing driver whose contextual read
records trace absence despite a traced queued refresh. After the new API exists,
verify the worker passes the original generation/valid header and explicit None,
including an awaited driver-owned queue without an OTel subscriber. Verify the
returned observation's independently chosen generation survives unchanged.

Exercise both actual queued refresh and outage-conflict dispatch; use a controlled
mailbox with real worker paths for deterministic cause checks and a FakeBroker
wire test for trace-bearing State/reply continuity. Legacy contextual-read and
plain-read drivers must work via the default chain. A guard-removal control at
each new dispatch site must produce its intended behavioral RED. A default-chain
test must fail if the wrapper bypasses an existing contextual read override.

Run focused then full SDK/black-box conformance tests (including unfenced
drivers), scoped strict Clippy, compiled tutorial examples, wait registry and
format/whitespace checks. Run affected original trace/generation controls and
the new dispatch controls sequentially, followed by restored checks. Two
sequential read-only design/source reviews; only same-family models available.

After the SDK is locally committed, repin all four DALI git dependencies to that
exact local revision and verify DALI before adding physical MQTT behavior. The
revision remains unpublished; no push/release/deployment is inferred.

## Parent self-review

Read all four worker read call sites and the trait's default chain, mailbox
coalesced read context selection, RequestContext equality/explicit-absence rules,
existing generation and trace tests, and current DALI service/proxy entry points.
The two request-owned sites are distinct from unsolicited proof/poll calls.
Returning a ContextualState does not expose the input request cause to a driver;
the similarly named existing method does not already solve this boundary.
The change cannot automatically stamp observations: original mapping authority
remains the resolving driver/service's responsibility.

Both sequential source-grounded design reviews approved this scope without
material findings. Four focused tests now pass. Before dispatch was changed,
both actual refresh/conflict worker regressions failed because the original
context hook was bypassed; the legacy default-chain test already passed.
The additional ephemeral FakeBroker test checks actual State and reply metadata
for valid supplied trace and explicit absence, across the driver's owned task
without an OTel subscriber. The unit read preserves a different observation
generation rather than adopting the request token.

## Final local verification

Both sequential read-only source reviews approved without material findings.
The restored kit suite passed **215 tests**, including tutorial examples and the
wait registry; full black-box conformance passed **174 tests**, including existing
unfenced drivers and process cases. All-target Clippy with warnings denied,
armv7 Linux musl library compilation, formatting and whitespace checks passed.
All **3 new controls** and **10 selected existing generation/trace controls**
failed for their intended behavioral assertions, then the restored full suites
passed again. The tables and ledgers preserve the selected rows and outcomes.

Initial verification rejected a redundant WAIT tag on a local delegation; it was
removed. The inherited RUST_LOG=warn also hid INFO shutdown records in two existing
conformance assertions. As already documented in the managed-service spec, the
corrected/restored conformance runs explicitly used RUST_LOG=info. No production
logging policy or process code changed. The canonical bridge-development skill
now documents the new hook. No new Store catalog resource or skill name was added.

Reviews remain same-family, and no live broker, hardware, Logmon or deployment
was used. The shared revision remains local until separately published.

# Request identity and original expiry

Contributor implementation checkpoint for DMX v2 integration. This change extends
the managed-service base `d83fa224bf04216dcde4be4102b1b2dc98fd7010`; it is not a release pin
or the complete recovery runtime.

`RequestContext` owns the original monotonic deadline and an optional canonical
operation digest alongside the existing trace metadata. The metadata shares one
copy-on-write allocation across context clones. Generation alone remains the
equality, ordering and hash key; metadata cannot grant mapping authority.

The wire parser captures MQTT expiry at original pump receipt. Device policy applies
the configured lifetime cap before queuing. Momentary and read mailbox entry points
take the earlier of a supplied bound and a context's original expiry. Coalesced reads
discard expired callers before selecting the latest surviving cause and earliest
surviving deadline. Atomic property writes and their validation/conflict-read paths
use the earliest surviving property deadline. No queue starts a new TTL.

A supplied `request-id` must occur exactly once and contain 64 lowercase hexadecimal
characters. Its operation-specific digest meaning belongs to the driver. Valid IDs
survive malformed payload/other-header rejection and are echoed in Error JSON;
invalid or duplicated IDs are not echoed. Headerless requests retain their behavior.
Trace ownership remains independent, including explicit absence on replies.

Verification covers context cloning/identity, canonical IDs, wire capture, capped
ingress, every ingress queue shape, rejection paths, driver rejection of combined
writes, surviving coalesced readers and an ephemeral FakeBroker command/error round
trip. Reproducible negative controls live in
[`request-metadata-controls.toml`](../../../mqtt-bridge-kit/docs/request-metadata-controls.toml).
The existing trace controls are re-anchored where execution-context selection changed.

At this checkpoint the library suite passes 227/227, the isolated-broker conformance
suite passes 176/176 with `RUST_LOG=info`, and all-target Clippy passes with warnings
denied. The first conformance run had two shutdown-log assertions fail because the
session supplied `RUST_LOG=warn`; both passed with INFO, followed by a full passing run.
Eight metadata controls and eight trace controls detect their declared regressions.

The opt-in recovery subscription barrier, exact recovery model replacement and DMX
managed service adapter remain separate work. This checkpoint supplies their expiry
and error-correlation prerequisites; it does not assert recovery readiness.

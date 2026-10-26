# Managed recovery handshake checkpoint

Contributor record for the DMX v2 runtime prerequisites, based on the published
request-metadata checkpoint `0eb9021`. This is an implementation increment, not a
completed recovery runtime or release pin.

Implemented paths:

- An exact retained snapshot subscription and its matched successful SUBACK precede
  a separate nonce subscription; its matched successful SUBACK closes replay.
- Every connection starts with closed output/admission. A bounded latest-replay watch
  distinguishes absence from invalid oversized input. Old connection capabilities and
  consumed replay notifications cannot release a new connection.
- The engine submits a complete RecoveryImage and waits for actual runtime installation.
  Cancelled, invalid, expired and stale installations do not open admission. The original
  request expiry is checked again at the serialized installation boundary.
- Exact replacement clears the model, acknowledged writes, frozen readback/conflict
  caches, worker setup caches and queued work even when mapping generations are unchanged.
  New model generations carry the initiating trace, independently of the old request token.
- RecoveryOnly has an empty target table/model/setup and keeps targetless recovery
  commands available. Promotion uses the same current-connection installation path.
- Shutdown does not flush queued engine output before recovery installation.
- A ten-second initial engine-decision bound retries the connection without
  restarting the service; expired decisions cannot commit while copying the model.

The snapshot byte cap and complete model/setup seed cap are independent: max_bytes
bounds retained input; MAX_PACKET_SIZE bounds the encoded runtime seed. The latter
must accommodate duplicated configuration views without shrinking the approved DMX
configuration limit. This does not establish the complete service-wide memory bound.

The FakeBroker runtime fixture holds the barrier SUBACK, tests blocked admission and
publication, reconnects without restarting the managed service, rejects old completion,
uses recovery commands, and promotes RecoveryOnly to Ready. Additional tests exercise
exact model/cache retirement, deadline/cancellation, explicit trace ownership, and
shutdown before installation. Reproducible mutations are in
[`recovery-controls.toml`](../../../mqtt-bridge-kit/docs/recovery-controls.toml).

Self-review corrections: a two-broker test reproduced shared recovery ownership when
a configured builder was cloned. Coordinators are now allocated per managed start;
the test and deliberate shared-owner mutation distinguish the correction. A separate
test reproduced loss of restored feedback overrides. Exact setup now seeds them under
the mailbox lock and clears them when the selected setup omits the override.
The bootstrap decision timeout and final-swap expiry check have dedicated probes.
Recovery-only refusals now preserve correlated command replies, and builtin refresh
cannot bypass the recovery command path. Fenced assume was already refused by the
existing session implementation; it does not require another recovery-specific guard.

Mosquitto 2.1.2 acceptance verifies the retained document arrives before the later
barrier acknowledgment, with a temporary configuration, an ephemeral loopback listener,
no persistence and explicit broker teardown. Run it with `--features mosquitto-acceptance`
and the `isolated_mosquitto_orders` test filter; it is not an implicit external dependency
of ordinary library tests. The final ordinary library run passed 245/245 with no
skips. All 24 recovery controls and 45 existing completion, generation, managed-service,
queued-read, request-metadata and trace controls failed as expected when their guards
were deliberately removed. All-target clippy, including the Mosquitto feature, and
format checks pass. The isolated Mosquitto acceptance test passed 1/1; its filter
intentionally excludes the other library tests. The unchanged isolated-broker
conformance suite also passed 176/176 with no skips at INFO logging.

Remaining feature work includes latest-snapshot publication and exact PUBACK receipts,
isolated deployment-broker ordering verification, bounded total request payloads,
service-wide memory accounting and the DMX managed engine adapter. No real DMX hardware
or shared MQTT broker is used by these checks.

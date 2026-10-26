# Request-mailbox byte admission checkpoint

The approved DMX v2 resource policy needs an aggregate request budget, independent
of MQTT forwarding and terminal answers. The SDK now offers the opt-in fixed-bridge
setting `request_queue_bytes(limit)`; zero is a startup usage error. Legacy callers
remain on the original unlimited byte path unless they opt in. The DMX adapter will
select its approved budget when it is implemented.

| Enumeration command | Inputs and states | Resource end / refusal |
|---|---|---|
| `rg -n 'post_state_then\|post_target_then' mqtt-bridge-kit/src/mailbox.rs` | Property and whole-target candidates; live, replay, held, in-flight and unproven values; copy and conflict-basis pins | Candidate check credits replacements. A refusal restores the previous mailbox before any assumption withdrawal. Applied/proven/failed/retired values release their accounted contents; pins retain their existing lifetimes. |
| `rg -n 'post_config_with_context\|configured_with_context' mqtt-bridge-kit/src/mailbox.rs` | Latest pending configuration and active configuration | Rejected replacement keeps the previous configuration; acceptance/failure/retirement ends the in-flight reservation. Feedback metadata has its separate configuration/model bound. |
| `rg -n 'post_command_with_context\|in_flight_command_bytes\|command_done' mqtt-bridge-kit/src/mailbox.rs` | Pending command, parameters owned by the worker, original request context and response metadata | Entire admitted charge transfers to active work, without growth at dispatch. Completion/disconnection/retirement releases it. |
| `rg -n 'post_read_with_context\|Entry::Read' mqtt-bridge-kit/src/mailbox.rs` | Refresh/custom read parameters and every coalesced caller's context/reply | Each additional caller is admitted; refusal preserves earlier callers and parameters. Dispatch retains the admitted charge until completion. |
| `rg -n 'Posted::Refused' mqtt-bridge-kit/src/session.rs` | Desired, commands, refresh and custom reads | Existing request-id and response-topic/correlation handling produces explicit byte-budget refusals. A full reply budget uses MQTT-only retry. |

Accounting uses saturating arithmetic, fixed per-entry costs and the maximum of
serialized JSON and decoded content estimates. This is an accounted content limit,
not an allocator/RSS measurement. Mailbox admission does not bound temporary JSON
parsing, driver allocations, the output model or aggregate engine/recovery storage.
The candidate backup exists only for opted-in mailboxes and cannot exceed already
admitted content; the incoming candidate itself is an additional temporary allocation.

Regression work first observed the three primary behavioral tests failing without
admission. A later dispatch test exposed a double charge introduced by the first
implementation; dispatch now retains the original reservation unchanged. Ingress
tests exercise the actual route/parser, including original request-id, refresh
correlation pressure and explicit read refusals. A launched bridge on an owned
loopback broker refuses an oversized command, accepts a smaller subsequent command,
and keeps its engine ticking without reconnecting MQTT.

The expanded all-target runtime run also exposed omitted wait registrations and raw
locks in the earlier recovery/buffering checkpoints. These now use the project's
lock-order wrappers; recovery, overflow, identity and installation waits are registered.
The generated waiter list includes the current managed-service recovery launcher.
Final verification: 290/290 runtime tests across library, integration and example
targets, and unchanged compatibility 176/176, both with zero skips. All-target
clippy including the Mosquitto feature, formatting and diff checks pass. The wait
registry reports zero findings. All nine new guard-removal controls fail for their
expected assertions; 124 anchors across the existing and new SDK control tables
plan-check without rot. Earlier controls were not rerun. The runtime suite took
12.1 s, compatibility 41.2 s, and the nine controls 51.9 s, excluding compilation.
No physical DMX output or home broker was used, and the DMX adapter is still pending.

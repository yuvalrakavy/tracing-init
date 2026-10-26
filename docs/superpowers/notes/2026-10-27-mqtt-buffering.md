# MQTT buffering implementation checkpoint

The owner approved MQTT-only retry on queue exhaustion on 2026-10-27, while lighting
and animations continue. This resolves the question recorded at snapshot checkpoint
`9cc6f8f`; it does not authorize restarting the managed engine on MQTT overload.

The first increment adds opt-in BufferLimits for incoming forwarding and terminal
answers. Each launch allocates its own coordinator. Incoming reservations remain
with the actual pump queue. Terminal-answer reservations move from Reports through
sealed batches/client enqueue and remain until the exact outgoing sequence/packet
is acknowledged. Negative PUBACK ends the reservation too. Overflow has an independent,
cancellation-safe wakeup in both the session select and its in-progress work select.
It closes the connection without flushing and uses existing paced retries.

| Boundary / source enumeration | Inputs / states | Resource end |
|---|---|---|
| `rg -n 'fn buffers\|buffers:' mqtt-bridge-kit/src/{process,host}.rs` | Optional positive limits; builder clones carry settings, not live counters | Allocate per launch; absent settings preserve legacy paths |
| `rg -n 'reserve_incoming\|Queued\|fn recv' mqtt-bridge-kit/src/{buffering,pump}.rs` | Decoded PUBLISH contents plus metadata; aggregate admission, current connection | Dequeue/drop releases; capacity exhaustion ends connection without waiting |
| `rg -n 'reserve_reply\|answer_charges\|charged' mqtt-bridge-kit/src/{buffering,reports,session}.rs` | Terminal Error/reply plus context, topic, correlation and fixed overhead | Charge transfers with actual answer; cannot be released by sealing or enqueue |
| `rg -n 'fn sent\|fn written\|fn acknowledged' mqtt-bridge-kit/src/buffering.rs` | Enqueued ordinal, actual packet id; current opaque connection capability | Exact ACK releases; begin/drop clears old in-flight reservations |
| `rg -n 'overflowed\|Step::Overflow' mqtt-bridge-kit/src/{buffering,session}.rs` | Incoming/reply overflow, including before waiter registration or first startup | Immediate MQTT-only end, even inside blocked publication; no managed service restart |
| `rg -n 'claim_loss\|broker_failure\|discard_buffered_answers' mqtt-bridge-kit/src/{buffering,pump,reports,session,lifecycle}.rs` | Current connection only; loss snapshot claimed once | Old callbacks cannot re-snapshot a successor; local pressure does not mark broker unreachable |

No payload history accumulates: counters are checked before reserve, arithmetic is
checked, and reservations release exactly once through ownership. This covers these
two queues only. The request mailbox, unsolicited/model storage and aggregate decoded
engine/recovery allocation admission remain incomplete. Accounted bytes are not an
allocator/RSS measurement; transport parsing also precedes this queue admission.

The two initial full-runtime probes failed before implementation, then passed. A
stronger ACK-held fixture first exposed bootstrap setup that had withheld ACKs before
the readback unsubscribe; it now waits for that unsubscribe. A deliberate removal of
the in-progress-work interrupt exposed a false-positive burst fixture: it could trip
before publication blocked. The replacement uses the real session enqueue counter,
one broker in-flight slot and 100 client request slots before triggering overflow.
Self-review also reproduced and corrected local pressure being reported as an
unreachable broker. A zero-limit startup control exposed a fixture that omitted
the required instance: it now supplies otherwise valid settings and checks the
specific buffer-limit error. All 23 new controls then produced expected failures.
The restored library passed 267/267 and unchanged conformance passed 176/176,
both with zero skips. All-target clippy, including the Mosquitto feature, formatting
and diff checks pass. Existing control tables were plan-checked with zero rotted
anchors; their earlier mutation runs were carried rather than repeated.
Mosquitto 2.1.2 acceptance also passed 1/1 with buffering enabled alongside the
recovery handshake, exact snapshot receipt and fresh-subscriber retained readback.
That targeted run intentionally excluded the other library tests and used only an
owned ephemeral loopback broker with persistence disabled and explicit teardown.

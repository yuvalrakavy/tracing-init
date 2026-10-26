# Recovery snapshot publication checkpoint

Contributor record for the DMX v2 runtime prerequisites after the published handshake
checkpoint `bb39bf4`. The managed DMX adapter and complete service-wide bounds remain
separate implementation work.

`RecoveryImage.snapshot` selects an immutable encoded State carrier with exact runtime
installation. Only a current Ready connection accepts later offers through
`RecoveryHandle.publish`. One replaceable pending snapshot and one immutable in-flight
snapshot bound publication history. Existing session/client, packet-id budget and output
rotation perform publication; there is no second MQTT connection. A publication timer
and the actual enqueue timestamp keep enqueues at least 100 ms apart.

The pump's opt-in events bind the actual outgoing publish ordinal to its packet id.
Only that packet's PUBACK credits the immutable snapshot identity/revision. Negative
PUBACKs report failure; enqueue alone does not establish broker acceptance. Receipt
storage holds metadata only, avoiding another retained payload copy. Loss and exact
installation clear old pending/in-flight work and receipts. RecoveryOnly cannot
overwrite retained recovery evidence.

Snapshot byte storage is shared through capture, pending, in-flight and runtime handoff.
Complete MQTT packet size includes topic, QoS 1 packet identifier, properties and fixed
header. Local and broker-advertised ceilings produce an explicit receipt fault before
sending, avoiding reconnect loops. Other publication kinds and the aggregate service
queue/memory bound are not covered by this increment. Snapshot publication preserves
its explicit trace, including absence, without copying an array's mapping authority.

The first runtime publication regression failed because the selected snapshot never
reached MQTT, then passed after integration. Runtime tests exercise pending coalescing,
held acknowledgments, exact receipts, reconnect fencing, timer wakeup, negative PUBACK,
continued ordinary control and both packet ceilings. The FakeBroker now advertises
packet ceilings without hiding a sender violation and can refuse v5 QoS 1 publications
under a topic prefix without retaining or routing them.

The opt-in Mosquitto 2.1.2 acceptance test also verifies an exact successful receipt
and a fresh subscriber's retained readback after the managed bridge stops. It passed
1/1 on an owned ephemeral loopback broker with persistence disabled and explicit
client/broker teardown. This verifies the installed local broker, not an untested
deployment broker or broker disk durability. Final ordinary library tests passed
256/256, unchanged compatibility tests 176/176 and the broker fixture's own tests
20/20, all with zero skips. All-target clippy with the Mosquitto feature, formatting
and diff checks pass. All 23 new snapshot controls, 24 recovery controls and eight
affected trace controls produced their expected failures, then the restored library
and compatibility suites passed. The previous checkpoint's other 37 controls were
not rerun for this increment; their evidence remains attached to `bb39bf4`.

## State and boundary enumeration

This table records the inspected production callers; tests also call internal
coordinator methods directly to distinguish wrong sequence/packet cases.

| Boundary | Inputs and allowed state | Effect / resource end | Enumeration command |
|---|---|---|---|
| Engine offer | Current connection, Ready, nonempty snapshot within max_bytes | Replace one pending value; stale/RecoveryOnly/oversized offers preserve it | `rg -n 'fn publish\|fn offer\|validate_snapshot' mqtt-bridge-kit/src/recovery{.rs,/publication.rs}` |
| Exact install | Current replay or RecoveryOnly promotion; complete image, original deadline, optional snapshot | Replace pending/in-flight/receipt with the selected image only after model installation; RecoveryOnly requires no snapshot | `rg -n 'install_pending\|publication.reset\|self.snapshot' mqtt-bridge-kit/src/recovery.rs` |
| Pending | One latest payload; no in-flight value; due at enqueue + 100 ms, initially immediate | Lifecycle deadline wakes the rotation; transfer to one flight; connection loss clears it | `rg -n 'publication_deadline\|take_publication\|queue_recovery_snapshot' mqtt-bridge-kit/src/{recovery/publication,lifecycle}.rs` |
| Queued flight | Immutable identity/revision/bytes, sequence and packet unset | Existing rotation and packet-id budget hand it to the existing client; session loss or bounded shutdown ends a stalled send | `rg -n 'publication_sent\|type Sending\|fn publish' mqtt-bridge-kit/src/{session,recovery/publication}.rs` |
| Packet admission | Full topic/QoS1/properties/payload size; min(local 1 MiB, advertised peer cap), absent peer cap means local cap | Oversize becomes a receipt fault without enqueue or retry; no inferred broker acceptance | `rg -n 'packet_size\|maximum_packet_size\|max_packet.store' mqtt-bridge-kit/src/{session,lifecycle,pump}.rs` |
| Written flight | Exact enqueued publish ordinal and actual nonzero packet id | Bind packet only for the selected flight's sequence | `rg -n 'Publishing\|publication_written' mqtt-bridge-kit/src/{pump,session,lifecycle,recovery/publication}.rs` |
| Awaiting PUBACK | One flight and at most one pending value; no caller waits on a future here | Matching ACK finishes it; negative reason is failure. Missing ACK retains bounded storage until link loss/stop; the engine keeps ticking | `rg -n 'PubAck\|publication_ack\|finish_publication' mqtt-bridge-kit/src/{pump,session,lifecycle,recovery/publication}.rs` |
| Receipt | Latest metadata only: connection, immutable id, revision, optional packet, success/fault | Replaced by the next result; cleared by connection loss and exact install. Shutdown receipt delivery is best effort | `rg -n 'RecoveryReceipt\|fn receipt\|fn reset' mqtt-bridge-kit/src/recovery{.rs,/publication.rs}` |
| Connection reset | Begin and lost, including stale completed capabilities | Pending, flight and receipt cleared; latest engine state must accompany the next install | `rg -n 'fn begin\|fn lost\|publication.reset' mqtt-bridge-kit/src/recovery.rs` |

The limits have a fixed point: repeated offers replace the same single pending
slot, and receipts retain no payload. No revision history accumulates. Byte clones
share immutable storage; ordinary runtime JSON models and request queues are outside
this snapshot-only proof and still require the service-wide admission work.

## Queue-exhaustion policy question

The next increment must bound the mailbox, coalesced read callers, pump forwarding
and terminal-answer backlog. The existing pump uses an unbounded forwarding channel,
and Reports deliberately keeps terminal answers outside unsolicited-report caps
(`rg -n 'unbounded_channel\|answers:' mqtt-bridge-kit/src/{pump,reports}.rs`).
Ordinary overload can be refused before engine mutation while reply capacity remains.
If the broker stalls and that capacity is exhausted too, retaining every reply would
break the memory bound.

Proposed disposition, awaiting owner confirmation: end and pace the MQTT session
when its bounded reply capacity is exhausted, with an explicit queue-exhaustion
record. Keep the managed lighting engine, current output and animation timeline
running. Outstanding unanswered calls may time out; accepted engine changes remain
authoritative and request ordering protects reconnect retries. Do not implement this
exception to ordinary overload refusal until the owner resolves the question.

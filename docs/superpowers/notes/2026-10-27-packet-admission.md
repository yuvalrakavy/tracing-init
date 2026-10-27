# Complete outgoing MQTT packet admission

The recovery snapshot previously had explicit size admission; other output could
reach rumqttc, end its poll, and prevent later valid messages from being delivered.
With the explicit `Bridge::packet_limits()` policy, all PUBLISH paths check complete packet bytes before enqueue, using the
minimum of the 1 MiB local limit and the current connection's advertised ceiling.
The size calculation delegates topic/properties to rumqttc and adds payload/length
header widths without cloning the payload. Tests compare it with actual QoS 1
packet sizes across 127/128 and 16383/16384 boundaries, large correlation metadata,
traceparent and the local ceiling.

| Enumeration command | Boundary / outcome |
|---|---|
| `rg -n 'packet_admitted\|async fn emit' mqtt-bridge-kit/src/session.rs` | Answers, model, unsolicited events, cleanup/retractions, replay and direct lifecycle publications; reject before client enqueue, with INFO `mqtt_packet_rejected` |
| `rg -n 'firsts -=\|publication_rejected\|packet_admitted' mqtt-bridge-kit/src/session.rs` | Refusal consumes the rotation quantum, releases the answer charge and first-publication barrier, and produces no enqueue or cleanup receipt; snapshot gets its exact failure receipt |
| `rg -n 'fn packet_size\|packet_fault' mqtt-bridge-kit/src/lifecycle.rs` | SUBSCRIBE/UNSUBSCRIBE; use rumqttc's exact size, latch unusable control as a connection fault before allocating a packet id |
| `rg -n 'packet_fault' mqtt-bridge-kit/src/lifecycle.rs` | Closed request/output gates; no new controls, ACK-driven bootstrap, overdue timer or activation; independent engine remains alive; reset only on a new connection |

A control-size fault clears pending lifecycle bookkeeping and closes recovery
installation/publication capabilities. Existing broker-side filters may still
exist, but incoming requests are ignored while admission is suspended. The socket
continues to be polled for normal protocol health; this fault does not manufacture
a broker outage or reconnect. Fix the ceiling/topic setup before reconnecting.
The broker's ceiling is unknown before CONNACK, so CONNECT has no peer-ceiling
check. Keepalive and disconnect retain their existing bounded protocol handling.

The policy is opt-in, like the other DMX resource limits. A compatibility fixture
deliberately publishes over 1 MiB to fill a stopped broker's socket buffers; the
unchanged conformance behavior must remain valid for bridges that do not opt in.
Recovery snapshots keep their previous always-on size admission. A public-builder
test proves the opt-in reaches the actual session and that a control-size fault
does not restart the managed engine.

The production-lane regression was observed failing before implementation: an
oversized output blocked later small state. Other probes cover all output queues,
direct lifecycle output, exact boundaries, receipt/reservation release, and control
fault/no-deadline behavior using owned loopback brokers. A self-review regression
also reproduced stale retained Active=true when bootstrap faulted before its first
activation; the size-fault path now clears it whenever Active=false fits the ceiling.
Nine new guard-removal
controls each fail for the expected assertion. Final suite and compatibility
results are recorded in the tracker; no physical DMX or home broker is used.

Final verification: runtime 297/297 across library, integration and examples
(15.6 s), unchanged compatibility 176/176 (41.2 s), both with zero skips.
All-target clippy including Mosquitto support, formatting/diff and wait-registry
checks pass. All nine new controls have the expected RED result. The 133 current
SDK control anchors plan-check without rot; old mutation results were carried.
The initial compatibility run exposed the over-1MiB stress fixture; the opt-in
policy restores its unchanged pass. A fresh Mosquitto run was not needed for this
packet path; the earlier combined acceptance remains prior checkpoint evidence.

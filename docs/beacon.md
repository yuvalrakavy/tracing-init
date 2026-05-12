# OTel collector beacon protocol

`tracing-init`'s OpenTelemetry feature includes a small UDP multicast
listener that lets a collector — or anything acting on a collector's
behalf — flip every consumer's circuit breaker in well under a second,
without waiting for the next reprobe window.

This document specifies the wire format, defaults, and operational
considerations so external tools can interoperate.

## Default endpoint

| Setting       | Default            |
|---------------|--------------------|
| Multicast group | `239.255.77.1`   |
| Port          | `4399`             |
| Transport     | UDP (IPv4)         |
| Encoding      | UTF-8 plain text   |

Both the group and the port can be overridden per app via the TOML
configuration:

```toml
[logging.otel]
beacon_group = "239.255.77.2"
beacon_port  = 4401
```

The group lives in the IPv4 administratively scoped block
(`239.0.0.0/8`), so it does not leak past site boundaries on properly
configured routers. The default port is arbitrary; pick whatever fits
your environment.

## Messages

Each datagram carries exactly one ASCII message, optionally followed by a
trailing newline (`\n`). Whitespace around the message is trimmed.

| Message        | Effect on the receiving circuit breaker                                                              |
|----------------|------------------------------------------------------------------------------------------------------|
| `OTEL:ONLINE`  | Force the breaker **closed**. Resets the failure count and clears the "offline" log-once flag.       |
| `OTEL:OFFLINE` | Force the breaker **open**. Marks the reprobe timer fresh so the next probe happens one interval out.|

Unknown messages are silently ignored. Implementations MUST NOT crash or
log on unrecognised content; the listener is best-effort.

There is no acknowledgement, retry, ordering, or sequence number. The
beacon is a hint — it accelerates the existing reprobe loop but never
replaces it. A consumer that misses a packet recovers via the regular
`reprobe_interval` (default 30 s).

## Producers

Typical producers:

- The OTel Collector's lifecycle hooks (e.g. `postStart` in Kubernetes;
  `RunAtLoad` / `ExitTimeOut` in launchd; `ExecStartPost` / `ExecStop` in
  systemd).
- A small sidecar process that watches the collector's TCP port and emits
  `OTEL:ONLINE` once it accepts connections, and `OTEL:OFFLINE` when it
  goes away.
- An operator-facing CLI: `echo -n OTEL:ONLINE | nc -u -b 239.255.77.1 4399`.

A minimal producer in shell:

```sh
# announce online
printf 'OTEL:ONLINE\n'  | socat - UDP4-DATAGRAM:239.255.77.1:4399
# announce offline
printf 'OTEL:OFFLINE\n' | socat - UDP4-DATAGRAM:239.255.77.1:4399
```

In Rust:

```rust
use std::net::UdpSocket;

let sock = UdpSocket::bind("0.0.0.0:0")?;
sock.send_to(b"OTEL:ONLINE\n", "239.255.77.1:4399")?;
```

## Consumers (what `tracing-init` does)

On startup with the `otel` feature enabled, `tracing-init`:

1. Creates a UDPv4 socket via `socket2`, sets `SO_REUSEADDR` (and
   `SO_REUSEPORT` on Unix) so multiple apps on the same host can share
   the port.
2. Binds to `0.0.0.0:<beacon_port>` and joins the configured multicast
   group on the unspecified interface.
3. Spawns a `tokio` task that loops on `recv_from`, trims the payload,
   and dispatches `force_close` / `force_open` on the shared
   `CircuitState`.

The task is aborted when the `TracingGuard` is dropped.

## Operational notes

- **Sub-second recovery.** Local-network multicast latency is microseconds
  in practice; the circuit transitions on the receive thread, so end-to-end
  latency from beacon to "exports flowing" is bounded by the OTel SDK's
  batch interval (default 5 s, configurable via the SDK).
- **No reverse channel.** The beacon is one-way; the collector does not
  learn which clients exist. If you need an inventory, log the OTel
  service.name on the collector side.
- **Security.** The beacon is unauthenticated and unencrypted. Anyone who
  can send multicast on the configured group can flip the circuit. Two
  considerations:
  - The worst-case effect of a hostile `OTEL:OFFLINE` is dropped traces
    until the next reprobe — no application data is leaked.
  - The worst-case effect of a hostile `OTEL:ONLINE` is one wasted export
    against the actual endpoint, which the SDK handles normally.
  Both are bounded; the trade-off is intentional for a development/edge
  protocol. If you need stronger guarantees, run `tracing-init` with the
  beacon disabled (custom `beacon_group` on a private address) and rely
  on the reprobe loop alone.
- **Containers.** Multicast across container network namespaces requires
  the network plugin to forward IGMP. The simplest deployment is to put
  the broker (collector) and the consumers on the same host network or
  the same `--network host`.

## Versioning

This document describes beacon protocol **v1**. Future versions, if any,
will add new messages alongside the existing two; consumers will
continue ignoring unknown messages as described above.

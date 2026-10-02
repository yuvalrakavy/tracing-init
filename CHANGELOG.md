# Changelog

All notable changes to `tracing-init` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`lock-order`: `sync::Condvar`** for its std `Mutex`. A wait gives up the guard's class,
  checks the re-acquisition's order against what the thread still holds BEFORE waiting (std
  re-takes the mutex before `wait` returns, so a later check would come too late), and takes
  the class back on a normal or poisoned return. `wait`, `wait_while`, `wait_timeout`,
  `wait_timeout_while`.
- **`wait-lint`: name collisions.** An `.await` on one of this code's async method names with a
  receiver other than `self` is a finding until the registry declares the name `wait-methods`
  (a dependency's: every call becomes a wait) or `local-methods` (this code's). A declared wait
  method already won over this code's name; a fixture now pins it. A stale `local-methods` name
  is a finding.
- **`wait-lint`: no empty scan.** `Report::files_scanned`; `assert_registered` and the CLI
  refuse a scan that read no file or found no wait.
- **`wait-lint`: raw-lock escapes.** `raw-locks = forbid` also refuses a raw `std`/`parking_lot`
  `Condvar`, and a glob import of or alias for `std::sync`, `tokio::sync` or `parking_lot`.
- **Telemetry-loss latch**, read with `telemetry_loss()`: answers whether this process
  silently dropped spans or log records. The OpenTelemetry SDK keeps its drop counts
  private and reports loss only through two internal `tracing` events, so a small layer
  watches for them and latches the first-drop time — the process can now be asked the
  question instead of it being answered by whoever remembers the right log query. Never
  cleared: the SDK reports only the FIRST drop until shutdown, so a latch is exactly as
  much as can be known while the process runs, and deriving a lost COUNT is unsound under
  sampling, pending work and shutdown timeouts. The layer carries its own `Targets` filter
  (`opentelemetry_sdk` at WARN) and is attached independently of every destination: behind
  a destination's filter it could be left permanently clear, and attached plain it would
  report no max-level hint, dragging the whole subscriber to TRACE and undoing static level
  skipping. Requires the `opentelemetry` crate's `internal-logs` feature, which is on by
  default — a dependency graph that disables default features there without re-adding it
  makes the latch permanently silent.
- **Monotonic exporter-availability counters** on the circuit breaker, reported through the
  same `telemetry_loss()` call: `export_failures_total`, `batches_discarded_total` and
  `first_failure_at`. The existing `failure_count` is the CONSECUTIVE count the breaker
  acts on and is zeroed at four sites, so a transient outage that recovered before the
  process ended previously left no trace at all. Availability sits beside loss and is never
  folded into it: an unreachable collector is a condition the breaker handles by design,
  while a full queue is telemetry that no longer exists.
- New optional `tokio-console` feature: a `console-subscriber` layer wired
  in alongside the existing destinations behind the destination character
  `t`. Adds `.log_to_tokio_console(bool)` and `.tokio_console_bind(&str)`
  on the builder and a `[logging.tokio_console]` TOML section. Requires
  the consuming crate to build with `RUSTFLAGS="--cfg tokio_unstable"` to
  emit events.
- Project documentation overhaul ahead of the open-source release: expanded
  README, CONTRIBUTING guide, CHANGELOG, beacon-protocol spec, and Medium
  intro article.

### Fixed
- **`lock-order`: `Condvar::wait_while` / `wait_timeout_while`** loop over the wrapper's own
  `wait` / `wait_timeout`, so the predicate (which runs under the mutex) runs holding the guard's
  record: a lock taken in it is ordered after the condvar's mutex, and re-taking that mutex there
  is reported. `wait_timeout_while` still bounds the whole wait.
- **`lock-order`: a guard gives up its record before its lock**, in every `sync` and
  `tokio_sync` guard: the next holder never finds the last one still on file (a wedge report
  naming a holder that had let go; a flaky holder count).
- **`wait-lint`: raw-lock escapes.** `use std::sync::{self as s}`, `extern crate parking_lot as
  pl`, a `pub use` of a lock module, `crate::sync::Mutex` through a binding anywhere in the
  crate, a glob through a bound module (`use std::sync; use sync::*`), parking_lot's
  `ReentrantMutex`, `FairMutex` and `const_*` constructors, and `lock_api`'s locks are refused.
  Paths are read through the file's `use`s, so `use lock_order::sync; sync::Mutex` is no longer
  reported as raw.
- **`wait-lint`: an imported function's name.** A call is read through the file's `use`s: `use
  dep::publish; publish(..).await` is the dependency's wait even where this code defines an
  `async fn publish`; a bare call to this code's async fn in a file that glob-imports from a
  dependency is a collision for `wait-methods` / `local-methods`. `check_dirs` reads each crate's
  `Cargo.toml` for its own name, so a `src/bin/` binary's `use my_crate::…` stays this code.
- **`mqtt-test-broker`:** an ack's hold-or-send decision and its push are one step under the
  `held` lock (a release could strand an ack); dropping the broker now closes its connections
  (they kept acknowledging and held the client's socket open); the docs say QoS 2 is
  acknowledged, never held.
- **`wait-lint`: more raw-lock escapes** (the 3b re-review, T2). A lock module bound at the root
  and rebound in a child through the crate (`use crate::sync;` or `use super::sync;`, then
  `sync::Mutex`) is followed to `std::sync`; `parking_lot::lock_api`'s locks are raw locks; and a
  glob of a module that holds a lock module (`use std::*`, `pub use tokio::*`) is refused, since it
  binds `sync` without naming it.
- **`wait-lint`: a dependency's function behind a local module** (the 3b re-review, T3). A call is
  followed module by module through what each module defines, binds by `use` and glob-imports, so
  where this code defines its own `async fn sleep`, tokio's `sleep` is still a wait when it arrives
  through `use super::*` over a parent's import, through a local module's `pub use`
  (`net::sleep`, `crate::net::sleep`, or a `use` of either), or as `self::sleep` over the file's
  own import. A module path into a module whose dependency glob may hold the name is a collision.
- **`mqtt-test-broker`:** a connection's writer is aborted with its connection, also while it is
  blocked writing to a client that stopped reading; detached, it held the client's socket open
  past the broker's drop and delivered everything still queued once the client read again (the
  3b re-review, T4). The ack-race test waits until the acker has reached the `held` lock (an
  arrival count kept under test) instead of sleeping 100 ms, so its control sees a slow acker too
  (T1).

## [0.2.0]

### Added
- OpenTelemetry feature (`otel`) with OTLP/HTTP and optional OTLP/gRPC
  (`otel-grpc`) transports for both traces and logs.
- Circuit-breaker wrapper around the OTLP exporters: silently drops exports
  while the collector is unreachable instead of flooding stderr; logs a single
  status line when going offline/online; re-probes on a configurable interval.
- UDP multicast beacon listener (`OTEL:ONLINE` / `OTEL:OFFLINE`) so the
  circuit breaker can react in well under a second when a collector becomes
  available or goes away.
- Automatic suppression of the OTel log bridge when the GELF layer is active
  (avoids duplicate log delivery; GELF carries the OTel trace/span IDs).
- Per-destination configuration (level, filter, format, ANSI, timestamps,
  target, thread names, file/line, span events).
- TOML configuration model with per-app overrides, per-destination overrides,
  and destination modifiers (`-f+o`).
- `LOG_CONFIG` environment variable to choose a TOML file at runtime.
- Destination-keyed builder API (`.level("console", …)`, `.format("file", …)`,
  …) alongside the legacy `log_to_*` methods.
- `TracingGuard` with `summary()` / `Display`; flushes OTel and file buffers
  on drop with a 1-second OTel shutdown cap.

### Changed
- Bumped `opentelemetry` / `opentelemetry_sdk` / `opentelemetry-otlp` to 0.31
  and `tracing-opentelemetry` to 0.32.

## [0.1.0]

- Initial release: console + rotating file + GELF over UDP, with `tracing`
  subscriber initialization and a TOML configuration file.

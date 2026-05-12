# Changelog

All notable changes to `tracing-init` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- New optional `tokio-console` feature: a `console-subscriber` layer wired
  in alongside the existing destinations behind the destination character
  `t`. Adds `.log_to_tokio_console(bool)` and `.tokio_console_bind(&str)`
  on the builder and a `[logging.tokio_console]` TOML section. Requires
  the consuming crate to build with `RUSTFLAGS="--cfg tokio_unstable"` to
  emit events.
- Project documentation overhaul ahead of the open-source release: expanded
  README, CONTRIBUTING guide, CHANGELOG, beacon-protocol spec, and Medium
  intro article.

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

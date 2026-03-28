# OpenTelemetry Support & GELF Enrichment Design

**Date:** 2026-03-28
**Status:** Approved

## Overview

Add OpenTelemetry support (traces + logs) to `tracing-init` and enrich the GELF layer with span context information. This positions `tracing-init` as a complete tracing initialization library that integrates with the OpenTelemetry ecosystem while maintaining its core value proposition: simple setup, zero complex wiring.

### Goals

- Export traces and logs via OTLP (HTTP and gRPC) to any OpenTelemetry Collector
- Enrich GELF messages with span context (trace_id, span_id, attributes, etc.) for logmon correlation
- Support per-destination log levels and filters
- Expose console/file display options (format, ansi, timestamps, etc.)
- Gate heavyweight dependencies behind compile-time features
- Migrate config from flat keys to nested TOML sections

### Non-Goals

- Sampling configuration (export everything; let the Collector handle sampling)
- Metrics export (traces + logs only)
- Production log aggregation (this is a development-time companion)

## Config Structure

The TOML config moves from flat keys to nested sections. All fields are optional with sensible defaults.

```toml
[logging]
destination = "cfo"                   # c=console, f=file, g=gelf, o=otel
level = "info"                        # default for all destinations
filter = "my_crate=debug,tower=warn"  # default EnvFilter for all
service_name = "my-service"           # OTel resource + GELF _service

[logging.console]
level = "debug"
filter = "my_crate=trace"
ansi = true                           # default: auto-detect terminal
format = "pretty"                     # full | compact | pretty | json
timestamps = true
target = true
thread_names = false
file_line = false
span_events = "new,close"            # new | close | active | none

[logging.file]
level = "info"
filter = "tower=off"
path = "logs"
prefix = "myapp"
rotation = "d:3"                     # daily, keep 3 backups
format = "json"                      # full | compact | json (no pretty)
timestamps = true
target = true
thread_names = false
file_line = false
span_events = "none"

[logging.gelf]
level = "warn"
filter = "my_crate=info"
address = "localhost:12201"

[logging.otel]                       # requires "otel" feature
level = "error"
filter = "my_crate=warn"
endpoint = "http://localhost:4318"
transport = "http"                   # http | grpc (grpc requires "otel-grpc" feature)

[logging.otel.resource]              # additional OTel resource attributes
"service.version" = "1.2.3"
"deployment.environment" = "staging"

[logging.myapp]                      # per-app overrides (existing feature)
destination = "-f+o"
level = "debug"
```

### Config Changes from Current

- **Flat to nested:** `file_path` becomes `[logging.file] path`, `server` becomes `[logging.gelf] address`, etc.
- **New sections:** `[logging.console]`, `[logging.otel]`
- **New fields:** `service_name`, per-destination `level`/`filter`, display options
- **Destination character `s` replaced by `g`** for GELF (matches section name `[logging.gelf]`)
- **New destination character `o`** for OpenTelemetry
- **Destination modifiers** updated accordingly: `"-g+o"`, `"+g"`, etc.
- **Breaking:** single consumer (store-server), migration is trivial

### Per-App Overrides

Per-app override sections (`[logging.myapp]`) inherit from the base `[logging]` section and can override any field. Destination modifiers (`-f+o`) continue to work as before.

Per-app sections also support nested destination overrides:

```toml
[logging]
level = "info"

[logging.console]
format = "pretty"

[logging.myapp]
destination = "co"
level = "debug"

[logging.myapp.console]
format = "json"
level = "trace"

[logging.myapp.otel]
level = "error"
```

**Inheritance chain (most specific wins):**
1. `[logging.myapp.console]` — app + destination specific
2. `[logging.myapp]` — app-level defaults
3. `[logging.console]` — destination defaults
4. `[logging]` — global defaults

For each field, the first level in this chain that defines it wins. For example, if `[logging.console]` sets `format = "pretty"` and `[logging.myapp]` sets `level = "warn"`, then myapp's console gets `format = "pretty"` (from level 3) and `level = "warn"` (from level 2).

This supports use cases where a single config file (e.g., `server.toml`) configures logging for multiple services (the main server and sub-services like `ht_server`) with different per-destination settings.

**Reserved section names:** `console`, `file`, `gelf`, `otel` are reserved for destination configuration. Within `[logging.otel]`, `resource` is reserved for OTel resource attributes. App names must not collide with reserved names. If a collision occurs, the section is treated as destination config, not an app override.

## Init Return Type

`init()` returns a `TracingGuard` instead of a plain `String`:

```rust
let guard = TracingInit::builder("myapp")
    .destination("cgo")
    .init()?;

println!("Logging: {}", guard.summary());

// guard must be held for the lifetime of the application.
// On drop: flushes pending OTel spans/logs, flushes file appender.
```

`TracingGuard` holds:
- The `tracing-appender` `WorkerGuard` (if file logging is active)
- The OTel `TracerProvider` and `LoggerProvider` (if OTel is active)
- The configuration summary string

On drop, it calls `TracerProvider::shutdown()` and `LoggerProvider::shutdown()` to flush pending data, and drops the file appender guard to flush buffered writes.

`TracingGuard` implements `Display`, delegating to `summary()`, so both `guard.summary()` and `format!("{guard}")` work.

### Summary Content

The summary string describes the active logging setup. Example:

```
console (pretty, DEBUG), file logs/myapp.log (json, INFO, daily:3), gelf localhost:12201 (WARN), otel http://localhost:4318 (ERROR), service: my-service, config: server.toml
```

Each active destination shows its key settings (format, level, endpoint). Inactive destinations are omitted.

## Builder API

The builder uses a destination-keyed API to avoid method proliferation:

```rust
TracingInit::builder("myapp")
    .service_name("my-service")
    .destination("cfo")
    .level("*", Level::INFO)
    .level("console", Level::DEBUG)
    .filter("file", "tower=off")
    .format("console", Format::Pretty)
    .format("file", Format::Json)
    .ansi("console", true)
    .timestamps("*", true)
    .target("*", true)
    .thread_names("console", true)
    .file_line("*", false)
    .span_events("console", SpanEvents::All)
    .otel_endpoint("http://localhost:4318")
    .otel_transport(Transport::Http)
    .otel_resource_attribute("service.version", "1.2.3")
    .file_path("logs")
    .file_rotation("d:3")
    .init()?;
```

### Legacy Methods (Kept)

The existing `log_to_console(bool)`, `log_to_file(bool)` methods are retained. `log_to_server(bool)` is renamed to `log_to_gelf_server(bool)` to match the `g` destination character. A new `log_to_otel(bool)` method is added (feature-gated behind `otel`). These are equivalent to setting individual destination flags and take the same precedence as other builder calls.

### `service_name` and `app_name`

- `app_name` (required, passed to `builder()`) — used for per-app TOML lookup, file prefix, and GELF `_app` field
- `service_name` (optional) — used for OTel resource identity and GELF `_service` field. Defaults to `app_name` if not set.
- Internal sub-services within a process should use span attributes (e.g., `tracing::info_span!("request", service = "ht_server")`) rather than separate `service_name` values. These flow into GELF as `_span_service` and into OTel span attributes automatically.

### Level and Filter Interaction

- `level()` sets the base log level for a destination (e.g., `level("*", Level::INFO)`)
- `filter()` sets an `EnvFilter` directive for a destination (e.g., `filter("console", "my_crate=debug,tower=warn")`)
- When both are set for a destination, the filter is used as the `EnvFilter` with the level as the default directive. This means the level acts as a floor — everything at that level or above passes, and the filter provides additional per-module overrides on top.

### API Conventions

- `"*"` sets the default for all destinations
- Specific destination names (`"console"`, `"file"`, `"gelf"`, `"otel"`) override the default
- Destination-specific settings (`otel_endpoint`, `file_path`, etc.) remain as direct methods since they only apply to one destination
- **Precedence:** explicit builder calls > env vars > TOML config > defaults

### Enums

```rust
pub enum Format { Full, Compact, Pretty, Json }
pub enum Transport { Http, Grpc }  // feature-gated

bitflags! {
    pub struct SpanEvents: u8 {
        const NEW    = 0b001;
        const CLOSE  = 0b010;
        const ACTIVE = 0b100;
        const NONE   = 0b000;
        const ALL    = Self::NEW.bits() | Self::CLOSE.bits() | Self::ACTIVE.bits();
    }
}
// TOML: span_events = "new,close" → SpanEvents::NEW | SpanEvents::CLOSE
```

## Environment Variables

Only variables that make sense as per-run overrides:

| Variable | Purpose |
|---|---|
| `LOG_DESTINATION` | Quick switch (e.g., add console for debugging) |
| `LOG_LEVEL` | Bump verbosity for a single run |
| `RUST_LOG` | Standard EnvFilter override |
| `LOG_CONFIG` | Point to a different config file |

Everything else (endpoints, formats, service name, per-destination levels) belongs in TOML or builder API.

**`RUST_LOG` interaction with per-destination filters:** `RUST_LOG` overrides the base `[logging]` filter only. Destinations that have their own explicit `filter` setting (via TOML or builder) keep their filter unchanged. This preserves carefully configured per-destination filters while allowing a quick per-run override of the default.

**Env vars and the `config` feature:** `LOG_DESTINATION`, `LOG_LEVEL`, and `RUST_LOG` always work regardless of the `config` feature — they don't require TOML or serde. `LOG_CONFIG` is silently ignored when the `config` feature is off (log a warning: "LOG_CONFIG set but 'config' feature not enabled — ignoring").

**Precedence:** builder > env vars > TOML > defaults (unchanged).

## Defaults

| Setting | Default |
|---|---|
| `destination` | (none — no destinations enabled) |
| `level` | `info` |
| `filter` | (none) |
| `service_name` | value of `app_name` |
| `console.ansi` | auto-detect (true if stdout is a terminal) |
| `console.format` | `full` |
| `file.format` | `full` |
| `file.path` | current directory |
| `file.prefix` | value of `app_name` |
| `file.rotation` | `d:3` (daily, 3 backups) |
| `gelf.address` | `localhost:12201` |
| `otel.endpoint` | `http://localhost:4318` |
| `otel.transport` | `http` |
| `timestamps` | `true` |
| `target` | `true` |
| `thread_names` | `false` |
| `file_line` | `false` |
| `span_events` | `none` |

## Architecture & Module Structure

```
src/
├── lib.rs              # TracingInit builder, layer assembly, public API
├── config.rs           # TOML parsing, env vars, precedence logic (feature: config)
├── gelf.rs             # GELF layer + span context enrichment (feature: gelf)
└── otel/               # (feature: otel)
    ├── mod.rs          # Shared config (endpoint, transport), provider setup
    ├── traces.rs       # OTel trace exporter layer
    └── logs.rs         # OTel log exporter layer
```

### Layer Assembly

The `init()` method assembles the subscriber as:

```
Registry
  + EnvFilter (console) + fmt::Layer (console)
  + EnvFilter (file) + fmt::Layer (file)          [feature: file]
  + EnvFilter (gelf) + GelfLayer                  [feature: gelf]
  + EnvFilter (otel) + OpenTelemetryLayer          [feature: otel]
  + EnvFilter (otel) + OtelLogLayer                [feature: otel]
```

Each layer gets its own `EnvFilter` via `Layer::with_filter()` (per-layer filtering), replacing the current global `EnvFilter` approach. This is a significant change from the current architecture where a single `EnvFilter` is added to the registry:

```rust
// Current: single global filter
registry.with(console).with(file).with(gelf).with(env_filter).init()

// New: per-layer filters
registry
    .with(console.with_filter(console_filter))
    .with(file.with_filter(file_filter))
    .with(gelf.with_filter(gelf_filter))
    .with(otel_traces.with_filter(otel_filter))
    .with(otel_logs.with_filter(otel_filter))
    .init()
```

### Shutdown

When `TracingGuard` is dropped, it performs an orderly shutdown:

1. Flush and shut down `TracerProvider` (exports pending spans)
2. Flush and shut down `LoggerProvider` (exports pending log records)
3. Drop the file appender `WorkerGuard` (flushes buffered file writes)

This ensures no data is lost on process exit. The guard must be held for the lifetime of the application — dropping it early stops all logging.

## GELF Enrichment

The `GelfLayer` currently sends basic event fields (message, level, timestamp, host). It will be enriched with all available span context.

### Additional Fields

**From the current span:**
- `_trace_id` — OTel trace ID (32-char hex), present only when `otel` feature is active
- `_span_id` — OTel span ID (16-char hex), present only when `otel` feature is active
- `_span_name` — name of the current span (available from tracing regardless of OTel)
- `_span_*` — span fields flattened as individual GELF additional fields (e.g., `_span_user_id`, `_span_request_method`)

**From builder/config:**
- `_app` — from `app_name` (existing, kept)
- `_service` — from `service_name` (defaults to `app_name` if not set)

**From tracing metadata:**
- `_target` — module path
- `_file` — source file
- `_line` — source line number

### Flattening Strategy

Span attributes are flattened as individual GELF additional fields (`_span_user_id = "123"`) rather than a nested JSON object. GELF additional fields are flat by convention and this is what logmon indexes on.

### Without OTel Feature

When the `otel` feature is not enabled, `_trace_id` and `_span_id` are simply absent. All other span context (name, attributes, service, target, file, line) remains available from the tracing span system.

## Feature Flags & Dependencies

```toml
[features]
default = ["config", "file", "gelf"]
config = ["dep:toml", "dep:serde"]
file = ["dep:tracing-appender"]
gelf = ["dep:serde_json", "dep:hostname"]
otel = [
    "dep:opentelemetry",
    "dep:opentelemetry_sdk",
    "dep:opentelemetry-otlp",
    "dep:tracing-opentelemetry",
    "dep:opentelemetry-appender-tracing",
]
otel-grpc = ["otel", "opentelemetry-otlp/grpc-tonic"]

[dependencies]
# Core (always)
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }

# Optional
tracing-appender = { version = "0.2", optional = true }
toml = { version = "0.8", optional = true }
serde = { version = "1.0", features = ["derive"], optional = true }
serde_json = { version = "1.0", optional = true }
hostname = { version = "0.4", optional = true }
opentelemetry = { version = "0.28", optional = true }
opentelemetry_sdk = { version = "0.28", features = ["rt-tokio"], optional = true }
opentelemetry-otlp = { version = "0.28", features = ["http-json"], optional = true }
tracing-opentelemetry = { version = "0.29", optional = true }
opentelemetry-appender-tracing = { version = "0.28", optional = true }
```

### Runtime Behavior When Feature Is Off

When a destination is requested but its feature is not enabled, log a warning at startup:
> "Destination 'o' requested but 'otel' feature not enabled — skipping"

No panics — graceful degradation.

### Error Handling

`init()` fails fast (returns `Err`) if any enabled destination cannot be set up:

- GELF address resolution fails → `Err`
- OTel endpoint is unreachable or exporter creation fails → `Err`
- File log directory cannot be created → `Err`
- Invalid filter directive → `Err`

No lenient/retry behavior. If a destination is requested, it must work at startup. This keeps the behavior predictable — the caller sees immediately whether logging is fully operational.

### Async Runtime

OTLP exporters require a tokio runtime. Consumers are expected to already be running tokio. No internal runtime spawning.

## Testing Strategy

### Unit Tests

- **Config parsing:** nested TOML sections, per-destination overrides, field inheritance
- **Destination modifiers:** `s` replaced by `g`, new `o` destination added
- **Per-destination level/filter:** resolution and precedence
- **Feature-gated config:** GELF/OTel fields ignored when feature off
- **Builder API:** `level("*", ...)` / `level("console", ...)` precedence and override behavior

### GELF Enrichment Tests

- Span context extraction: trace_id, span_id, span_name, attributes, service_name
- Behavior without OTel feature (no trace_id/span_id, other fields still present)
- Flattened span attributes format (`_span_*` fields)

### OTel Tests (Feature-Gated)

- Provider setup with HTTP transport
- Provider setup with gRPC transport (`otel-grpc` feature)
- Log exporter layer creation
- Graceful skip when `o` destination requested without feature

### Integration Testing

End-to-end integration testing is done through store-server + logmon, which exercises the full pipeline (tracing-init -> GELF/OTLP -> logmon). No standalone integration tests for export delivery.

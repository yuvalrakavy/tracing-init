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
destination = "cfo"                   # c=console, f=file, s=gelf, o=otel
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

[logging.myapp]                      # per-app overrides (existing feature)
destination = "-f+o"
level = "debug"
```

### Config Changes from Current

- **Flat to nested:** `file_path` becomes `[logging.file] path`, `server` becomes `[logging.gelf] address`, etc.
- **New sections:** `[logging.console]`, `[logging.otel]`
- **New fields:** `service_name`, per-destination `level`/`filter`, display options
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

**Inheritance chain:** `[logging.myapp.console]` inherits from `[logging.myapp]`, which inherits from `[logging.console]`, which inherits from `[logging]`. Most specific wins.

This supports use cases where a single config file (e.g., `server.toml`) configures logging for multiple services (the main server and sub-services like `ht_server`) with different per-destination settings.

**Reserved section names:** `console`, `file`, `gelf`, `otel` are reserved for destination configuration. App names must not collide with these. If a collision occurs, the section is treated as destination config, not an app override.

## Builder API

The builder uses a destination-keyed API to avoid method proliferation:

```rust
TracingInit::builder()
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
    .file_path("logs")
    .file_rotation("d:3")
    .init()
```

### API Conventions

- `"*"` sets the default for all destinations
- Specific destination names (`"console"`, `"file"`, `"gelf"`, `"otel"`) override the default
- Destination-specific settings (`otel_endpoint`, `file_path`, etc.) remain as direct methods since they only apply to one destination
- **Precedence:** explicit builder calls > env vars > TOML config > defaults

### Enums

```rust
pub enum Format { Full, Compact, Pretty, Json }
pub enum SpanEvents { None, New, Close, Active, All }  // or bitflags
pub enum Transport { Http, Grpc }  // feature-gated
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

**Precedence:** builder > env vars > TOML > defaults (unchanged).

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

Each layer gets its own `EnvFilter` constructed from the per-destination level and filter settings.

## GELF Enrichment

The `GelfLayer` currently sends basic event fields (message, level, timestamp, host). It will be enriched with all available span context.

### Additional Fields

**From the current span:**
- `_trace_id` — OTel trace ID (32-char hex), present only when `otel` feature is active
- `_span_id` — OTel span ID (16-char hex), present only when `otel` feature is active
- `_span_name` — name of the current span (available from tracing regardless of OTel)
- `_span_*` — span fields flattened as individual GELF additional fields (e.g., `_span_user_id`, `_span_request_method`)

**From builder/config:**
- `_service` — from `service_name`

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
```

### Runtime Behavior When Feature Is Off

When a destination is requested but its feature is not enabled, log a warning at startup:
> "Destination 'o' requested but 'otel' feature not enabled — skipping"

No panics — graceful degradation.

### Async Runtime

OTLP exporters require a tokio runtime. Consumers are expected to already be running tokio. No internal runtime spawning.

## Testing Strategy

### Unit Tests

- **Config parsing:** nested TOML sections, per-destination overrides, field inheritance
- **Destination modifiers:** existing logic extended with `o` destination
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

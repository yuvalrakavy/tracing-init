# tracing-init

Simple tracing subscriber initialization with TOML config, file rotation, GELF, and OpenTelemetry support.

## Quick Start

```rust
// Minimal -- auto-discovers logging.toml if present
let guard = tracing_init::TracingInit::builder("myapp").init().unwrap();
println!("Logging: {guard}");
// guard must be held for the application lifetime
```

## Features

- **Console logging** with configurable format (full, compact, pretty, JSON), ANSI colors, and display options
- **File logging** with configurable rotation (daily, hourly, minutely) and backup count
- **GELF output** over UDP with span context enrichment (trace ID, span ID, span attributes)
- **OpenTelemetry export** — traces and logs via OTLP (HTTP and gRPC)
- **Per-destination filtering** — independent log levels and filters per output
- **TOML configuration** with nested sections, per-app overrides, and destination modifiers
- **Feature-gated dependencies** — only pay for what you use

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `config` | yes | TOML configuration file support |
| `file` | yes | Rotating file logging |
| `gelf` | yes | GELF over UDP output |
| `otel` | no | OpenTelemetry trace + log export via OTLP/HTTP |
| `otel-grpc` | no | Adds gRPC transport for OTLP (pulls in `tonic`) |

```toml
# Cargo.toml
[dependencies]
tracing-init = "0.2"                          # default: config + file + gelf
tracing-init = { version = "0.2", features = ["otel"] }  # add OpenTelemetry
```

## TOML Configuration

```toml
[logging]
destination = "cfo"                   # c=console, f=file, g=gelf, o=otel
level = "info"                        # default for all destinations
filter = "my_crate=debug,tower=warn"  # default EnvFilter for all
service_name = "my-service"           # OTel resource + GELF _service

[logging.console]
level = "debug"
format = "pretty"                     # full | compact | pretty | json
ansi = true
timestamps = true
target = true
thread_names = false
file_line = false
span_events = "new,close"            # new | close | active | none | all

[logging.file]
level = "info"
path = "logs"
prefix = "myapp"
rotation = "d:3"                     # d=daily, h=hourly, m=minutely, n=never
format = "json"

[logging.gelf]
level = "warn"
address = "localhost:12201"

[logging.otel]
level = "error"
endpoint = "http://localhost:4318"
transport = "http"                   # http | grpc

[logging.otel.resource]
"service.version" = "1.2.3"
"deployment.environment" = "staging"

[logging.myapp]                      # per-app overrides
destination = "-f+o"                 # modifier: remove file, add otel
level = "debug"
```

### Destination Modifiers

- **Absolute**: `"cfo"` replaces the inherited value
- **Modifier**: `"-f"`, `"+o"`, `"-f+o"` adds/removes from inherited value

### Per-App Destination Overrides

```toml
[logging.myapp.console]
format = "json"
level = "trace"
```

Inheritance chain (most specific wins):
1. `[logging.myapp.console]` — app + destination
2. `[logging.myapp]` — app defaults
3. `[logging.console]` — destination defaults
4. `[logging]` — global defaults

## Environment Variables

Only per-run overrides:

| Variable | Purpose |
|----------|---------|
| `LOG_DESTINATION` | Quick switch (e.g., add console for debugging) |
| `LOG_LEVEL` | Bump verbosity for a single run |
| `RUST_LOG` | EnvFilter override (base filter only) |
| `LOG_CONFIG` | Point to a different config file |

## Precedence

1. Explicit builder calls (highest)
2. Environment variables
3. TOML config (app-specific over base)
4. Defaults (lowest)

## Builder API

### Destination-Keyed API

```rust
use tracing_init::{TracingInit, types::{Format, SpanEvents}};
use tracing::Level;

let guard = TracingInit::builder("myapp")
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
    .span_events("console", SpanEvents::ALL)
    .file_path("logs")
    .file_rotation("d:3")
    .init()
    .unwrap();

println!("Logging: {guard}");
```

### Legacy Methods

```rust
let guard = TracingInit::builder("myapp")
    .log_to_console(true)
    .log_to_file(true)
    .log_to_gelf_server(true)
    .no_auto_config_file()
    .ignore_environment_variables()
    .init()
    .unwrap();
```

### TracingGuard

`init()` returns a `TracingGuard` that must be held for the application lifetime. On drop, it flushes pending OTel data and file buffers.

```rust
let guard = TracingInit::builder("myapp")
    .destination("co")
    .init()?;
println!("{}", guard.summary());
// guard dropped at end of main → orderly shutdown
```

## GELF Enrichment

The GELF layer automatically includes span context in every log message:

| Field | Source |
|-------|--------|
| `_trace_id` | OTel trace ID (when `otel` feature enabled) |
| `_span_id` | OTel span ID (when `otel` feature enabled) |
| `_span_name` | Current span name |
| `_span_*` | Span field values (flattened) |
| `_service` | From `service_name` |
| `_app` | From `app_name` |
| `_target` | Module path |
| `_file`, `_line` | Source location |

## License

MIT

# tracing-init

[![Crates.io](https://img.shields.io/crates/v/tracing-init.svg)](https://crates.io/crates/tracing-init)
[![Documentation](https://docs.rs/tracing-init/badge.svg)](https://docs.rs/tracing-init)
[![CI](https://github.com/yuvalrakavy/tracing-init/actions/workflows/ci.yml/badge.svg)](https://github.com/yuvalrakavy/tracing-init/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

**One call to stand up a real-world `tracing` subscriber: console, rotating files, GELF over UDP, and OpenTelemetry — wired up from a TOML file, environment, or a fluent builder.**

```rust
let _guard = tracing_init::TracingInit::builder("myapp").init()?;
```

That's it. By default, `tracing-init` auto-discovers `logging.toml`, applies environment overrides, and configures every destination the file asked for. No more 50-line subscriber chains scattered across your binaries.

## Why this exists

Every non-trivial Rust service ends up writing the same code:

- A console layer with the right format for a TTY vs. a log collector.
- A rotating file appender for the operator who SSHs into the box at 3 a.m.
- A GELF or OTLP shipper so the events show up in Graylog / Loki / Tempo / Jaeger.
- An `EnvFilter` plus an environment-variable escape hatch.
- A guard you have to keep alive until `main` returns.

Each of those is easy on its own. Composing them — with independent log levels, output formats, and a sensible config file — adds up to a few hundred lines of `tracing-subscriber` builder calls per project. `tracing-init` is the result of doing that over and over, then extracting the boring bits into a crate.

It is intentionally small in scope: it does **not** invent a new logging facade, it does not require an async runtime unless you opt into OpenTelemetry, and it leaves your application code using the standard [`tracing`] macros.

## Highlights

- **Four production destinations, one builder** — console, rotating files, GELF/UDP, OTLP (HTTP or gRPC).
- **Plus an optional dev destination** — `tokio-console` for runtime task inspection.
- **Per-destination configuration** — independent levels, filters, formats, and span-event policies.
- **TOML-first** — declare the whole logging setup in `logging.toml`; the binary just calls `init()`.
- **Per-app overrides and destination modifiers** — share one config across a workspace of binaries.
- **GELF ↔ OpenTelemetry correlation** — span and trace IDs are injected into every GELF record automatically.
- **Resilient OTLP exporter** — built-in circuit breaker + multicast beacon so a missing collector never blocks startup, hangs shutdown, or floods stderr.
- **Feature-gated dependencies** — you only pull in `tokio`, `tonic`, OTel, `console-subscriber`, etc. if you actually use them.
- **No new logging APIs** — your application keeps using `tracing::{info,warn,error,instrument}`.

## Quick Start

```toml
# Cargo.toml
[dependencies]
tracing       = "0.1"
tracing-init  = "0.2"                                       # console + file + gelf
# tracing-init = { version = "0.2", features = ["otel"] }   # add OpenTelemetry
# tracing-init = { version = "0.2", features = ["tokio-console"] }  # add tokio-console
```

```rust
use tracing::info;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Holds OTel/file resources; drop at end of main to flush cleanly.
    let _guard = tracing_init::TracingInit::builder("myapp").init()?;

    info!(version = env!("CARGO_PKG_VERSION"), "service starting");
    run()?;
    info!("service shut down cleanly");
    Ok(())
}
# fn run() -> Result<(), Box<dyn std::error::Error>> { Ok(()) }
```

Drop a `logging.toml` next to your binary (or anywhere you point `LOG_CONFIG` at):

```toml
[logging]
destination = "cf"          # console + file
level       = "info"

[logging.console]
format = "pretty"

[logging.file]
path     = "logs"
rotation = "d:3"            # daily, keep 3 backups
```

Run it once and you have an ANSI-coloured console plus a `logs/myapp.YYYY-MM-DD.log` rotated daily. When you later need ship logs to Graylog, flip a flag in TOML — no Rust changes:

```toml
[logging]
destination = "cfg"
[logging.gelf]
address = "graylog.internal:12201"
```

## Feature flags

| Feature     | Default | Pulls in                                                | What you get                                  |
|-------------|---------|---------------------------------------------------------|-----------------------------------------------|
| `config`    | yes     | `toml`, `serde`                                          | TOML configuration files                       |
| `file`      | yes     | `tracing-appender`                                       | Rotating file appender                         |
| `gelf`      | yes     | `serde_json`, `hostname`                                 | GELF 1.1 over UDP                              |
| `otel`      | no      | `opentelemetry`, `opentelemetry_sdk`, `tokio`, …         | OTLP/HTTP traces + logs, circuit breaker, beacon |
| `otel-grpc` | no      | adds `tonic` to `opentelemetry-otlp`                     | gRPC transport for OTLP                        |
| `tokio-console` | no  | `console-subscriber`                                     | `tokio-console` instrumentation layer (requires `RUSTFLAGS="--cfg tokio_unstable"` in the consuming crate) |

`default = ["config", "file", "gelf"]`. Disable defaults if you want a minimal console-only build:

```toml
tracing-init = { version = "0.2", default-features = false }
```

## Configuration model

`tracing-init` reconciles four sources, in order of precedence:

1. **Explicit builder calls** — anything you set via `.destination(…)`, `.level(…)`, etc.
2. **Environment variables** — `LOG_DESTINATION`, `LOG_LEVEL`, `RUST_LOG`, `LOG_CONFIG`.
3. **TOML configuration** — app-specific sections override base sections.
4. **Built-in defaults** — sane choices that work without any config.

### TOML reference

```toml
[logging]
destination  = "cfo"                 # c=console, f=file, g=gelf, o=otel
level        = "info"                # default for all destinations
filter       = "my_crate=debug,tower=warn"
service_name = "my-service"          # OTel resource + GELF _service field

[logging.console]
level        = "debug"
format       = "pretty"              # full | compact | pretty | json
ansi         = true
timestamps   = true
target       = true
thread_names = false
file_line    = false
span_events  = "new,close"           # new | close | active | none | all

[logging.file]
level    = "info"
path     = "logs"
prefix   = "myapp"
rotation = "d:3"                     # d=daily, h=hourly, m=minutely, n=never
format   = "json"

[logging.gelf]
level   = "warn"
address = "localhost:12201"

[logging.otel]
level     = "error"
endpoint  = "http://localhost:4318"
transport = "http"                   # http | grpc (requires otel-grpc feature)

[logging.otel.resource]
"service.version"        = "1.2.3"
"deployment.environment" = "staging"

# Optional — only consulted when the `tokio-console` feature is enabled.
[logging.tokio_console]
bind = "127.0.0.1:6669"              # default; override or omit

# Per-app overrides — useful in a workspace where several binaries share one TOML.
[logging.myapp]
destination = "-f+o"                 # modifier: drop file, add otel
level       = "debug"

[logging.myapp.console]
format = "json"                      # per-app, per-destination override
```

### Destination strings

A destination is a string of single-character flags: `c` (console), `f` (file), `g` (gelf), `o` (otel), `t` (tokio-console).

- **Absolute** — `"cfo"` replaces the inherited value.
- **Modifier** — `"-f"`, `"+o"`, `"-f+o"` apply on top of the inherited value.

This means a single workspace config can say "everyone gets console + file" and a single app can opt into "also OTel" without restating the rest. Adding `t` is the same shape: add to the destination, enable the `tokio-console` feature, and build the consumer with `RUSTFLAGS="--cfg tokio_unstable"`.

### Inheritance chain

Most specific wins:

1. `[logging.<app>.<destination>]`
2. `[logging.<app>]`
3. `[logging.<destination>]`
4. `[logging]`

### Environment variables

| Variable          | Purpose                                                                                |
|-------------------|----------------------------------------------------------------------------------------|
| `LOG_DESTINATION` | Per-run destination string of `c`/`f`/`g`/`o`/`t` (e.g. add `c` for an interactive run) |
| `LOG_LEVEL`       | Bump verbosity for a single invocation                                                 |
| `RUST_LOG`        | Standard `EnvFilter` directives (base filter only)                                     |
| `LOG_CONFIG`      | Path to a TOML file to use instead of `logging.toml`                                   |

## Builder API

For tests, examples, or apps that don't want TOML at all:

```rust
use tracing::Level;
use tracing_init::{TracingInit, types::{Format, SpanEvents}};

let _guard = TracingInit::builder("myapp")
    .service_name("my-service")
    .destination("cfo")
    .level("*", Level::INFO)
    .level("console", Level::DEBUG)
    .filter("file", "tower=off")
    .format("console", Format::Pretty)
    .format("file", Format::Json)
    .span_events("console", SpanEvents::ALL)
    .file_path("logs")
    .file_rotation("d:3")
    .no_auto_config_file()
    .ignore_environment_variables()
    .init()?;
```

The legacy boolean methods are still around for backwards compatibility:

```rust
let _guard = TracingInit::builder("myapp")
    .log_to_console(true)
    .log_to_file(true)
    .log_to_gelf_server(true)
    .init()?;
```

## The guard

`init()` returns a `TracingGuard` you have to hold for the lifetime of your program:

```rust
let guard = TracingInit::builder("myapp").destination("co").init()?;
println!("{}", guard.summary());
// guard dropped at end of main → flush OTel batch processors and file buffers
```

`Display` and `Debug` are implemented; printing the guard prints a one-line summary of every active destination, which is handy as the first log line of a service.

On drop the guard:

1. Aborts the OTel beacon listener (if any).
2. Calls `provider.shutdown_with_timeout(1s)` on the tracer and logger providers. We cap shutdown at one second so an unreachable collector cannot hang `main` — the circuit breaker has already filtered the queue.
3. Drops the file appender's worker guard, flushing buffered writes.

## GELF enrichment

When the GELF layer is active, every record carries the surrounding span context:

| Field        | Source                                                                |
|--------------|-----------------------------------------------------------------------|
| `_trace_id`  | OTel trace ID (when the `otel` feature is enabled)                    |
| `_span_id`   | OTel span ID (when the `otel` feature is enabled)                     |
| `_span_name` | Name of the current span                                              |
| `_span_*`    | Span fields, flattened with a `_span_` prefix                         |
| `_service`   | `service_name`                                                         |
| `_app`       | `app_name`                                                             |
| `_target`    | Module path                                                            |
| `_file`, `_line` | Source location                                                   |

The result: when you ship GELF to a collector you also get the OTel trace/span IDs as searchable fields — so a log line found in Graylog can be pivoted to the corresponding trace in Tempo/Jaeger, and vice versa. You get cross-tool correlation without standing up an OTel log pipeline.

If both GELF and the OTel log bridge would be active, `tracing-init` skips the OTel log layer to avoid duplicate delivery — GELF already has the trace context.

## OpenTelemetry: collector-friendly by design

The `otel` feature wraps each OTLP exporter in a small circuit breaker:

- After `N` consecutive failed exports (default 3), the breaker opens and exports are silently dropped instead of retried — no more "tcp connect: Connection refused" floods in stderr while you fix the collector.
- A single status line is printed when the breaker opens (`OTel collector not online…`) and when it closes again (`OTel collector online, sending traces`).
- While open, one request is allowed through every `reprobe_interval_secs` (default 30 s) to detect recovery.

For instant detection without waiting for the reprobe window, the runtime also joins a UDP multicast group (default `239.255.77.1:4399`) and listens for plain-text `OTEL:ONLINE\n` / `OTEL:OFFLINE\n` packets. A collector that publishes those beacons can flip every app on the network from "circuit open" to "exporting" in well under a second.

Configure both from TOML:

```toml
[logging.otel]
endpoint           = "http://otel-collector:4318"
reprobe_interval   = 30      # seconds
failure_threshold  = 3
beacon_group       = "239.255.77.1"
beacon_port        = 4399
```

The beacon wire format is documented in [`docs/beacon.md`](docs/beacon.md) so external producers (collector lifecycle hooks, sidecars, ops scripts) can emit `OTEL:ONLINE` / `OTEL:OFFLINE` packets and flip every app on the network in well under a second.

This makes `tracing-init` a pragmatic choice for environments where the collector and the apps come up in any order (development laptops, k8s rollouts, edge devices).

## `tokio-console` (developer-time runtime inspection)

When the `tokio-console` feature is enabled and `t` is in the destination string, `tracing-init` adds a [`console-subscriber`](https://docs.rs/console-subscriber) layer that exposes per-task scheduling, polling, and resource-contention data to the standalone [`tokio-console`](https://github.com/tokio-rs/console) CLI.

```toml
[dependencies]
tracing-init = { version = "0.2", features = ["tokio-console"] }
```

```toml
# logging.toml
[logging]
destination = "ct"          # console + tokio-console

[logging.tokio_console]
bind = "127.0.0.1:6669"     # default; override or omit
```

```bash
# tokio's instrumentation API is gated behind a cfg, so the consuming
# crate must build with this flag for events to actually be emitted:
RUSTFLAGS="--cfg tokio_unstable" cargo run --features tokio-console

# in another terminal:
tokio-console     # cargo install tokio-console
```

Without the `tokio_unstable` flag the layer compiles fine but stays silent. With it set, the CLI connects on the configured port (default `127.0.0.1:6669`) and surfaces async runtime state — stuck tasks, suspicious await points, lock contention — that no log line will ever show you. The feature is off by default and pulls in `console-subscriber` only when enabled.

## Use with logmon

`tracing-init`'s GELF layer pairs naturally with [**logmon**](https://github.com/yuvalrakavy/logmon-mcp), a log-monitoring MCP server that ingests GELF (and OTLP) and exposes the buffered events to AI coding assistants via the [Model Context Protocol](https://modelcontextprotocol.io/).

```
your-binary  ──GELF/UDP──▶  logmon-broker  ──MCP/JSON-RPC──▶  Claude Code / Cursor / Windsurf / …
        (tracing-init)         (daemon)
```

A typical local setup:

1. Install `logmon-broker` and let it listen on `localhost:12201`.
2. Point your app at it with one line of TOML:

   ```toml
   [logging.gelf]
   address = "localhost:12201"
   ```

3. Tell your editor's AI assistant things like *"check the logs around the cache error"* or *"set up a trigger for panics"*. Because `tracing-init` injects span and trace IDs, the assistant can pivot from a single error log to the full surrounding trace.

You don't need logmon to use `tracing-init` — GELF works with Graylog, Vector, Fluent Bit, and anything else that speaks GELF — but if you want AI-native log access during development, this is the fastest path there.

## Examples

Three runnable examples live under [`examples/`](examples/):

| Command                                                            | What it shows                                                    |
|--------------------------------------------------------------------|------------------------------------------------------------------|
| `cargo run --example console_only`                                 | Smallest builder-only setup with pretty console output and spans |
| `cargo run --example full_toml`                                    | TOML-driven setup; edit `examples/full_toml.logging.toml` to flip destinations without recompiling |
| `cargo run --example otel_resilient --features otel`               | OTLP export with the circuit breaker and multicast beacon active — run with and without a collector to see the behaviour |

## Comparison with alternatives

- **`tracing-subscriber` directly** — the canonical answer for one binary with one output. `tracing-init` shines when you have several binaries, several destinations, and want the same TOML to drive them all.
- **`tracing-bunyan-formatter`, `tracing-loki`, `tracing-gelf`** — single-destination crates. You can absolutely combine them by hand; `tracing-init` is the prebuilt composition.
- **`tracing-opentelemetry`** — the underlying integration that `tracing-init` uses for the `otel` feature; we add the circuit breaker, the beacon, the GELF correlation, and the configuration model on top.
- **`console-subscriber`** — used directly by the `tokio-console` feature; `tracing-init` adds destination-string/TOML/builder wiring so it shares the same lifecycle and configuration surface as the other layers.

## MSRV and stability

The crate tracks recent stable Rust (2021 edition). The 0.x line may make breaking changes between minor versions; see [`CHANGELOG.md`](CHANGELOG.md).

## Contributing

Issues and PRs are welcome. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the short version: open an issue first for non-trivial changes, run `cargo test --all-features` before submitting, and prefer one logical change per PR.

## License

MIT — see [`LICENSE`](LICENSE).

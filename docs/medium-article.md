# Stop rewriting your `tracing` setup: meet `tracing-init`

*A small Rust crate that wires up console, files, GELF, and OpenTelemetry from one TOML file — and survives an offline collector without breaking a sweat.*

---

## The 200-line problem

If you've shipped more than one Rust service, you have probably written a
function like this — maybe twice, maybe twelve times:

```rust
fn init_tracing() -> WorkerGuard {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    let console_layer = fmt::layer()
        .pretty()
        .with_target(true)
        .with_writer(std::io::stdout);

    let file_appender = tracing_appender::rolling::daily("logs", "app.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = fmt::layer()
        .json()
        .with_writer(file_writer);

    let gelf_layer = MyGelfLayer::new("graylog:12201")?;

    // …and a few hundred lines later, an OTLP exporter,
    //  a circuit breaker, a flush-on-shutdown hook, env-var overrides…

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .with(gelf_layer)
        .init();

    guard
}
```

Every project starts with "I just need logs to stdout" and ends with the same
soup: console + rotating files + a structured log shipper, each with its own
filter and format. None of it is hard. All of it is repetitive. All of it
tends to subtly diverge between projects.

I'm going to walk you through
[`tracing-init`](https://github.com/yuvalrakavy/tracing-init) — a small crate
that replaces that 200-line setup with one builder call and a TOML file. The
piece I'm most happy with isn't the configuration model, though; it's the way
the OpenTelemetry integration behaves when the collector isn't there.

We'll also see how the same crate's GELF output pairs with
[**logmon**](https://github.com/yuvalrakavy/logmon-mcp) to give your AI coding
assistant direct access to your application's logs over the
[Model Context Protocol](https://modelcontextprotocol.io/).

## What "good" looks like

The shape I wanted from `tracing-init` is the smallest one that handles real
deployments:

- **One init call** in `main` — no exporting a logging module from a
  workspace crate, no copying boilerplate.
- **One TOML file** — declarative, diff-friendly, environment-aware. Ops
  people can change it without recompiling.
- **Per-destination knobs** — the console wants `pretty` and ANSI; the file
  wants `json` and a daily rotation; the GELF receiver wants a different
  level threshold. None of them should share `RUST_LOG`.
- **Standard `tracing` macros everywhere else** — no new logging API to
  learn or migrate to.

Here's the smallest possible app:

```rust
use tracing::info;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = tracing_init::TracingInit::builder("myapp").init()?;
    info!("ready");
    Ok(())
}
```

…with a `logging.toml` sitting next to the binary:

```toml
[logging]
destination = "cf"   # console + file
level       = "info"

[logging.console]
format = "pretty"

[logging.file]
path     = "logs"
rotation = "d:3"     # daily, keep 3 backups
```

That's the whole setup. When you later need to ship logs to Graylog, you
flip a flag in the file:

```toml
[logging]
destination = "cfg"
[logging.gelf]
address = "graylog.internal:12201"
```

No Rust changes. No redeploy of the logging layer. The same workflow
extends to OpenTelemetry by adding `o` to the destination and pulling in the
`otel` feature.

## The configuration model

The destination string is a string of single-character flags: `c` for
console, `f` for file, `g` for GELF, `o` for OTel. There are two ways to set
it:

- **Absolute** — `"cfo"` replaces the inherited value.
- **Modifier** — `"-f"`, `"+o"`, `"-f+o"` adds or removes flags on top of
  what was inherited.

Modifiers exist because a typical workspace has many binaries that should
share most of their logging configuration:

```toml
[logging]
destination = "cf"        # everyone gets console + file
level       = "info"

[logging.cli]             # the CLI tool wants pretty output, no file
destination = "-f"
[logging.cli.console]
format = "pretty"

[logging.api]             # the API server adds OTel
destination = "+o"
[logging.api.otel]
endpoint = "http://otel-collector:4318"
```

The inheritance chain is `[logging.<app>.<destination>]` → `[logging.<app>]`
→ `[logging.<destination>]` → `[logging]`, most specific wins. After a
couple of services you stop thinking about it — it just does what you
expect.

For tests and one-off scripts there's also a fluent builder:

```rust
use tracing::Level;
use tracing_init::{TracingInit, types::Format};

let _guard = TracingInit::builder("myapp")
    .destination("c")
    .level("*", Level::INFO)
    .level("console", Level::DEBUG)
    .format("console", Format::Pretty)
    .no_auto_config_file()
    .ignore_environment_variables()
    .init()?;
```

Same destinations, same per-destination knobs, expressed in Rust. The
precedence is: explicit builder calls > environment variables > TOML > the
crate's defaults.

## GELF, with trace context for free

The GELF layer is the part I keep coming back to. GELF (the
[Graylog Extended Log Format](https://go2docs.graylog.org/current/getting_in_log_data/gelf.html))
is a JSON-over-UDP protocol — simple, ubiquitous, and supported by basically
every log aggregator. `tracing-init`'s implementation is a couple of hundred
lines of boring blocking-UDP send. It's intentionally not async, doesn't
spawn threads, and doesn't queue: each event is serialized inline and
fire-and-forget'd to the socket.

The interesting part is what we put in each message. Every GELF record gets:

| Field        | Source                                                 |
|--------------|--------------------------------------------------------|
| `_trace_id`  | OTel trace ID (when the `otel` feature is enabled)     |
| `_span_id`   | OTel span ID (when the `otel` feature is enabled)      |
| `_span_name` | Name of the current span                               |
| `_span_*`    | Span fields, flattened with `_span_` prefix            |
| `_service`   | `service_name`                                          |
| `_app`       | `app_name`                                              |
| `_target`    | Module path                                             |
| `_file`, `_line` | Source location                                    |

That `_trace_id` / `_span_id` pair is the thing. When OpenTelemetry is also
active, every log line you ship to your GELF collector carries the same
trace ID you'd see in Tempo or Jaeger. You get cross-tool correlation
without standing up a separate OTel log pipeline — log search and
distributed tracing both point at the same identifier.

To avoid double-shipping, when both GELF and the OTel log bridge would be
active, `tracing-init` quietly disables the OTel log layer. Your logs go via
GELF (which already has the trace context) and your spans go via OTLP.

## OpenTelemetry that doesn't melt down when the collector is missing

Now for the part I'm most proud of.

If you've enabled `tracing-opentelemetry` with an OTLP exporter and the
collector wasn't running, you've seen what happens: the batch span processor
retries on every send, prints a fresh `tcp connect: Connection refused` to
stderr each time, and — worst of all — `provider.shutdown()` blocks for up
to 30 seconds at the end of `main` waiting for a queue it'll never flush.

That behavior is reasonable for a long-running production service where the
collector being down is a real incident. It is a terrible developer
experience for everyone else: CLIs, test runners, integration tests,
developers running the same binary on their laptops where the collector is
not part of the local stack.

`tracing-init` wraps every OTLP exporter — for traces and logs — in a small
circuit breaker:

```rust
pub struct CircuitState {
    state: AtomicU8,                 // CLOSED / OPEN / HALF_OPEN
    failure_count: AtomicU32,
    failure_threshold: u32,
    last_probe_ms: AtomicU64,
    reprobe_interval_ms: u64,
    has_logged_offline: AtomicBool,
    app_name: String,
}
```

The rules are deliberately simple:

- **Closed** — exports go through normally. Each failure increments a
  counter; on the *N*-th consecutive failure (default 3) the breaker opens.
- **Open** — exports return `Ok(())` immediately; nothing is sent. A single
  status line is printed *once* per offline period:

  > `[14:02:51] [myapp] OTel collector not online. Start the collector and traces will begin flowing within 30s`

- **Half-open** — after `reprobe_interval_secs` (default 30 s), exactly one
  export is allowed through. If it succeeds, the breaker closes and we log:

  > `[14:03:24] [myapp] OTel collector online, sending traces`

  If it fails, we go back to Open.

The whole thing is a few hundred lines of atomics; no locks, no extra
threads. Because the breaker turns failures into `Ok(())`, the OTel SDK's
queue never backs up — which means shutdown is also fast. We cap shutdown
explicitly:

```rust
const SHUTDOWN_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(1);
if let Some(ref provider) = self.tracer_provider {
    let _ = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
}
```

End result: when the collector is up, traces and logs flow normally. When
it isn't, the program prints one informational line, behaves as if OTel
weren't configured at all, and exits in <1 second instead of 30.

### The beacon: instant recovery

A 30-second reprobe interval is fine for a service that runs for hours. It
feels long when you're iterating locally: you start the collector, then sit
there waiting for the next reprobe.

So I added a small UDP multicast beacon. The crate joins
`239.255.77.1:4399` by default and listens for plain-text packets:

```
OTEL:ONLINE
OTEL:OFFLINE
```

Any process that wants to announce a collector lifecycle change can send
those packets — the OTel collector container's `postStart` hook, a
launchd job, a `make collector-up` recipe, anything. Receiving `OTEL:ONLINE`
forces every circuit on the network to close immediately; `OTEL:OFFLINE`
forces them open. No daemon-to-daemon protocol, no service discovery, no
DNS — just a multicast group and two strings.

```rust
"OTEL:ONLINE"  => state.force_close(),
"OTEL:OFFLINE" => state.force_open(),
_              => {} // ignore unknown messages
```

Configure both from TOML if you want different group/port or different
thresholds:

```toml
[logging.otel]
endpoint           = "http://otel-collector:4318"
reprobe_interval   = 30
failure_threshold  = 3
beacon_group       = "239.255.77.1"
beacon_port        = 4399
```

Together, the breaker and the beacon turn OTLP from a fragile dependency
into something you can leave on by default in every binary you write.

## Pairing with logmon: logs your AI assistant can read

`tracing-init` exists because I wanted *one* call that gives me real logs in
every Rust project. `logmon` exists because, once you have real GELF logs,
you can do something kind of magical: hand them to your AI coding assistant.

[**logmon**](https://github.com/yuvalrakavy/logmon-mcp) is a daemon that:

1. **Ingests** GELF (UDP + TCP) and OTLP (HTTP + gRPC) on local ports.
2. **Buffers** the most recent N events in memory, with per-session
   filters, triggers (e.g. "panic" matching, ERROR-or-above), and
   bookmarks.
3. **Exposes** all of that to MCP-compatible clients — Claude Code, Cursor,
   Windsurf, Codex, VS Code with Copilot — over a Unix-domain socket.

So your local stack ends up looking like this:

```
your-binary  ──GELF/UDP──▶  logmon-broker  ──MCP/JSON-RPC──▶  Claude Code / Cursor / Windsurf / …
        (tracing-init)         (daemon)
```

And the workflow ends up looking like this:

- You hit a bug.
- You say to your editor, *"check the logs around the cache error"*.
- The assistant calls `get_recent_logs(filter="connection refused, l>=warn")`
  via MCP, sees the actual error, looks at the `_trace_id` field, asks for
  the surrounding span context, and gives you a fix.
- You ask it to *"set up a trigger for panics"*; it calls `add_trigger`.
- Next time the binary panics, the assistant gets a notification with the
  surrounding logs.

Because `tracing-init` puts the OTel trace ID into every GELF record, the
assistant can pivot from a single log line to the full request trace
without any extra work on your part. Because the GELF layer is best-effort
UDP, none of this is in your application's critical path — there is no
runtime cost to the developer-experience improvement when logmon isn't
running.

You don't need logmon to use `tracing-init` — GELF goes happily to
Graylog, Vector, Fluent Bit, anything that speaks the protocol — but if
you're already pairing with an AI coding assistant, this is the fastest
way I know to give it useful access to your service's runtime.

## Bonus: peeking at the runtime with `tokio-console`

There is one more destination — `t` — that doesn't fit the "structured
logs" framing of the other four. With the `tokio-console` feature
enabled, `tracing-init` adds a [`console-subscriber`](https://docs.rs/console-subscriber)
layer that exposes per-task scheduling, polling, and resource-contention
data to the standalone [`tokio-console`](https://github.com/tokio-rs/console)
CLI:

```toml
[logging]
destination = "ct"        # console + tokio-console
```

```bash
RUSTFLAGS="--cfg tokio_unstable" cargo run --features tokio-console
tokio-console             # cargo install tokio-console
```

Tokio's instrumentation API is gated behind a `cfg`, so the consumer has
to opt in with `RUSTFLAGS="--cfg tokio_unstable"`; without it the layer
compiles but stays silent. That keeps the feature properly off-by-default
without invasive surgery on your build.

I keep it under the same destination machinery as the other layers
deliberately. When you suspect a stuck task or unexpected blocking, it's
the same `logging.toml` change as flipping on the file appender — no
parallel "debug runtime" wiring to maintain.

## What it doesn't try to do

A few non-goals, in case they save you from raising the wrong issue:

- **No new logging facade.** Application code uses `tracing::{info, warn,
  error, instrument}` and nothing else.
- **No bespoke format DSL.** Console/file format is `full`, `compact`,
  `pretty`, or `json` — exactly what `tracing-subscriber` already supports.
- **No log aggregation.** GELF is best-effort UDP; OTLP is `tracing-init`
  → SDK → exporter. The crate doesn't try to be a sink.
- **No mandatory async runtime.** The default features (`config`, `file`,
  `gelf`) don't pull in Tokio. The `otel` feature does, because the
  OpenTelemetry SDK does.

## Try it

```toml
[dependencies]
tracing       = "0.1"
tracing-init  = "0.2"                                              # console + file + gelf
# tracing-init = { version = "0.2", features = ["otel"] }          # add OpenTelemetry
# tracing-init = { version = "0.2", features = ["tokio-console"] } # add tokio-console
```

Repo: <https://github.com/yuvalrakavy/tracing-init>

The crate is MIT-licensed and intentionally small. Issues and PRs are
welcome — particularly real-world configuration files you wish had been
easier to express.

If you also want your AI assistant to read those logs, point
[logmon](https://github.com/yuvalrakavy/logmon-mcp) at the same port.

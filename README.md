# tracing-init

Simple tracing subscriber initialization with optional TOML configuration, file logging, and GELF server output.

## Quick Start

```rust
// Minimal -- auto-discovers logging.toml if present
let summary = tracing_init::TracingInit::builder("myapp").init().unwrap();
println!("Logging: {summary}");
```

## Features

- **Console logging** with ANSI color support
- **File logging** with configurable rotation (daily, hourly, minutely) and backup count
- **GELF server output** over UDP for centralized log collection (e.g., Graylog)
- **TOML configuration** with per-app overrides and destination modifiers
- **Environment variable** overrides
- **Upward directory search** for config files

## TOML Configuration

Load from a config file (falls back to `logging.toml` if no `[logging]` section):

```rust
let summary = tracing_init::TracingInit::builder("myapp")
    .config_file("server.toml")
    .init()
    .unwrap();
```

Or pass pre-parsed TOML:

```rust
let config: toml::Value = toml::from_str(&config_str).unwrap();
let summary = tracing_init::TracingInit::builder("myapp")
    .config_toml(&config)
    .init()
    .unwrap();
```

### TOML Structure

```toml
[logging]
destination = "csf"          # c=console, f=file, s=server
level = "info"
server = "localhost:12201"
file_path = "logs"
file_rotation = "d:3"        # d=daily, h=hourly, m=minutely, n=never

[logging.myapp]              # Per-app overrides
destination = "-f+s"         # Remove file, add server from base
level = "debug"
```

### Destination Modifiers

- **Absolute**: `"csf"` replaces the inherited value entirely
- **Modifier**: `"-f"`, `"+s"`, `"-f+s"` adds/removes individual destinations from the inherited value

## Environment Variables

| Variable | Description |
|----------|-------------|
| `LOG_DESTINATION` | Contains `c`, `f`, and/or `s` |
| `LOG_LEVEL` | `error`, `warn`, `info`, `debug`, `trace` |
| `LOG_FILE_PATH` | Path to the log file directory |
| `LOG_FILE_ROTATION` | `<rotation>[:<count>]` (d/h/m/n, default d:3) |
| `LOG_SERVER` | GELF server address (host:port) |
| `RUST_LOG` | Filter directive |
| `LOG_CONFIG` | Path to a TOML config file (used during auto-discovery) |

## Precedence (highest to lowest)

1. Explicit builder calls
2. Environment variables
3. TOML config (app-specific over base)
4. Defaults

## Builder API

```rust
tracing_init::TracingInit::builder("myapp")
    .log_to_console(true)
    .log_to_file(true)
    .log_to_server(true)
    .level(tracing::Level::DEBUG)
    .filter("myapp=debug,hyper=warn")
    .log_file_path("logs")
    .log_file_prefix("myapp")
    .log_file_rotation(tracing_appender::rolling::Rotation::DAILY)
    .log_file_backups(5)
    .log_server_address("graylog.example.com:12201")
    .config_file("server.toml")
    .no_auto_config_file()
    .ignore_environment_variables()
    .init()
    .unwrap();
```

## License

MIT

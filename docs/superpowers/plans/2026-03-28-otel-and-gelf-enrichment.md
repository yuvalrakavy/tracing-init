# OpenTelemetry Support & GELF Enrichment Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add OpenTelemetry trace/log export via OTLP, enrich GELF with span context, add per-destination filtering and display options, and migrate config from flat to nested TOML.

**Architecture:** The builder (`TracingInit`) is refactored to construct per-layer `EnvFilter`s instead of one global filter. Config parsing is rewritten for nested TOML sections with 4-level inheritance. New `otel/` module handles OTLP export behind a feature gate. GELF layer gains span context extraction. `init()` returns a `TracingGuard` that flushes on drop.

**Tech Stack:** `tracing`, `tracing-subscriber` (per-layer filtering), `tracing-appender`, `tracing-opentelemetry`, `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp`, `opentelemetry-appender-tracing`, `bitflags`, `serde_json`, `toml`, `serde`

**Spec:** `docs/superpowers/specs/2026-03-28-otel-and-gelf-enrichment-design.md`

---

## File Structure

### Files to Create

| File | Responsibility |
|---|---|
| `src/types.rs` | Public enums (`Format`, `Transport`) and `SpanEvents` bitflags |
| `src/dest_config.rs` | `DestinationConfig` struct — per-destination level/filter/display settings, resolution logic |
| `src/guard.rs` | `TracingGuard` struct — holds appender guard + OTel providers, flush-on-drop, summary |
| `src/otel/mod.rs` | OTel provider setup, endpoint/transport config (feature-gated) |
| `src/otel/traces.rs` | `tracing-opentelemetry` layer construction (feature-gated) |
| `src/otel/logs.rs` | `opentelemetry-appender-tracing` layer construction (feature-gated) |
| `src/tests/types_tests.rs` | Tests for enums and SpanEvents parsing |
| `src/tests/dest_config_tests.rs` | Tests for per-destination config resolution |
| `src/tests/guard_tests.rs` | Tests for TracingGuard summary output |
| `src/tests/gelf_enrichment_tests.rs` | Tests for GELF span context extraction |

### Files to Modify

| File | Changes |
|---|---|
| `Cargo.toml` | Feature flags, optional deps, `bitflags` |
| `src/lib.rs` | New builder API (destination-keyed methods), per-layer assembly, return `TracingGuard` |
| `src/config.rs` | Nested TOML parsing, `DestinationConfig` resolution, `g` replaces `s` |
| `src/gelf.rs` | Span context enrichment (`_trace_id`, `_span_id`, `_span_name`, `_span_*`, `_service`, `_target`) |
| `src/tests/mod.rs` | Add new test modules |
| `src/tests/config_tests.rs` | Update for nested TOML format, add new tests |
| `src/tests/integration_tests.rs` | Update for new API (`TracingGuard`, renamed methods) |

---

## Task 1: Update Cargo.toml with Feature Flags, Dependencies, and cfg Gates

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/lib.rs` (add cfg gates to existing module declarations)

**IMPORTANT:** Making existing deps optional will immediately break compilation because `src/lib.rs`, `src/config.rs`, and `src/gelf.rs` import them unconditionally. This task MUST also add `#[cfg]` gates to the existing module declarations and imports, or compilation will fail.

- [ ] **Step 1: Update Cargo.toml**

```toml
[package]
name = "tracing-init"
version = "0.2.0"
edition = "2021"
description = "Simple tracing subscriber initialization with TOML config, file rotation, GELF, and OpenTelemetry support"
license = "MIT"
repository = "https://github.com/yuvalrakavy/tracing-init"
keywords = ["tracing", "logging", "gelf", "opentelemetry", "subscriber"]
categories = ["development-tools::debugging"]

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
tracing = { version = "0.1", features = ["log"] }
tracing-subscriber = { version = "0.3", features = ["env-filter", "fmt", "json"] }
bitflags = "2"

# Optional - config
toml = { version = "0.8", optional = true }
serde = { version = "1", features = ["derive"], optional = true }

# Optional - file
tracing-appender = { version = "0.2", optional = true }

# Optional - gelf
serde_json = { version = "1", optional = true }
hostname = { version = "0.4", optional = true }

# Optional - otel
opentelemetry = { version = "0.28", optional = true }
opentelemetry_sdk = { version = "0.28", features = ["rt-tokio"], optional = true }
opentelemetry-otlp = { version = "0.28", features = ["http-json"], optional = true }
tracing-opentelemetry = { version = "0.29", optional = true }
opentelemetry-appender-tracing = { version = "0.28", optional = true }

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Add cfg gates to existing module declarations in `src/lib.rs`**

Change the existing module declarations:
```rust
// Before:
mod config;
mod gelf;

// After:
#[cfg(feature = "config")]
mod config;
#[cfg(feature = "gelf")]
mod gelf;
```

Also gate the `ConfigSource` enum and any `use` statements that reference `toml::Value` behind `#[cfg(feature = "config")]`. Gate `tracing_appender` imports behind `#[cfg(feature = "file")]`.

The existing code that uses these modules in `init()`, `apply_toml_config()`, etc. must also be wrapped in corresponding `#[cfg]` blocks. This is a mechanical but important step — every reference to `config::`, `gelf::`, `toml::`, `tracing_appender::`, `serde_json::`, and `hostname::` needs a feature gate.

- [ ] **Step 3: Verify it compiles with default features**

Run: `cargo check`
Expected: compiles cleanly (default features enable config, file, gelf — so all existing code paths are active)

- [ ] **Step 4: Verify it compiles with no default features**

Run: `cargo check --no-default-features`
Expected: compiles (console-only logging, all optional modules gated out)

- [ ] **Step 5: Verify it compiles with all features**

Run: `cargo check --all-features`
Expected: compiles (OTel crates download and resolve)

- [ ] **Step 6: Run existing tests**

Run: `cargo test`
Expected: all existing tests pass (default features match previous behavior)

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/lib.rs
git commit -m "feat: add feature flags, OTel dependencies, and cfg gates"
```

---

## Task 2: Create Types Module (Format, Transport, SpanEvents)

**Files:**
- Create: `src/types.rs`
- Create: `src/tests/types_tests.rs`
- Modify: `src/tests/mod.rs`
- Modify: `src/lib.rs` (add `mod types`)

- [ ] **Step 1: Write tests for SpanEvents parsing and Format/Transport Display**

Create `src/tests/types_tests.rs`:

```rust
use crate::types::{Format, SpanEvents};

#[test]
fn test_format_from_str() {
    assert_eq!("full".parse::<Format>().unwrap(), Format::Full);
    assert_eq!("compact".parse::<Format>().unwrap(), Format::Compact);
    assert_eq!("pretty".parse::<Format>().unwrap(), Format::Pretty);
    assert_eq!("json".parse::<Format>().unwrap(), Format::Json);
    assert!("invalid".parse::<Format>().is_err());
}

#[test]
fn test_format_case_insensitive() {
    assert_eq!("JSON".parse::<Format>().unwrap(), Format::Json);
    assert_eq!("Pretty".parse::<Format>().unwrap(), Format::Pretty);
}

#[test]
fn test_span_events_from_str_single() {
    assert_eq!("new".parse::<SpanEvents>().unwrap(), SpanEvents::NEW);
    assert_eq!("close".parse::<SpanEvents>().unwrap(), SpanEvents::CLOSE);
    assert_eq!("active".parse::<SpanEvents>().unwrap(), SpanEvents::ACTIVE);
    assert_eq!("none".parse::<SpanEvents>().unwrap(), SpanEvents::NONE);
    assert_eq!("all".parse::<SpanEvents>().unwrap(), SpanEvents::ALL);
}

#[test]
fn test_span_events_from_str_combined() {
    let events: SpanEvents = "new,close".parse().unwrap();
    assert_eq!(events, SpanEvents::NEW | SpanEvents::CLOSE);
}

#[test]
fn test_span_events_from_str_with_spaces() {
    let events: SpanEvents = "new, close".parse().unwrap();
    assert_eq!(events, SpanEvents::NEW | SpanEvents::CLOSE);
}

#[test]
fn test_span_events_from_str_invalid() {
    assert!("invalid".parse::<SpanEvents>().is_err());
}

#[cfg(feature = "otel")]
#[test]
fn test_transport_from_str() {
    use crate::types::Transport;
    assert_eq!("http".parse::<Transport>().unwrap(), Transport::Http);
    assert_eq!("grpc".parse::<Transport>().unwrap(), Transport::Grpc);
    assert!("websocket".parse::<Transport>().is_err());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib types_tests`
Expected: compilation error — `types` module doesn't exist

- [ ] **Step 3: Create `src/types.rs`**

```rust
use std::str::FromStr;

/// Output format for console and file logging layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Default verbose format with all fields.
    Full,
    /// Condensed single-line format.
    Compact,
    /// Multi-line colorful format (console only).
    Pretty,
    /// Structured JSON output.
    Json,
}

impl FromStr for Format {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "full" => Ok(Format::Full),
            "compact" => Ok(Format::Compact),
            "pretty" => Ok(Format::Pretty),
            "json" => Ok(Format::Json),
            other => Err(format!("unknown format: '{other}' (expected full, compact, pretty, or json)")),
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Format::Full => write!(f, "full"),
            Format::Compact => write!(f, "compact"),
            Format::Pretty => write!(f, "pretty"),
            Format::Json => write!(f, "json"),
        }
    }
}

/// OTLP transport protocol.
#[cfg(feature = "otel")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// HTTP/JSON (default, lighter dependency footprint).
    Http,
    /// gRPC via tonic (requires `otel-grpc` feature).
    Grpc,
}

#[cfg(feature = "otel")]
impl FromStr for Transport {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "http" => Ok(Transport::Http),
            "grpc" => Ok(Transport::Grpc),
            other => Err(format!("unknown transport: '{other}' (expected http or grpc)")),
        }
    }
}

#[cfg(feature = "otel")]
impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Http => write!(f, "http"),
            Transport::Grpc => write!(f, "grpc"),
        }
    }
}

bitflags::bitflags! {
    /// Which span lifecycle events to log.
    ///
    /// Maps to `tracing_subscriber::fmt::format::FmtSpan`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SpanEvents: u8 {
        /// Log when a span is created.
        const NEW    = 0b001;
        /// Log when a span is closed/dropped.
        const CLOSE  = 0b010;
        /// Log when a span is entered (becomes the active span).
        const ACTIVE = 0b100;
        /// No span events.
        const NONE   = 0b000;
        /// All span events.
        const ALL    = Self::NEW.bits() | Self::CLOSE.bits() | Self::ACTIVE.bits();
    }
}

impl FromStr for SpanEvents {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let trimmed = s.trim().to_lowercase();
        match trimmed.as_str() {
            "none" => return Ok(SpanEvents::NONE),
            "all" => return Ok(SpanEvents::ALL),
            _ => {}
        }

        let mut result = SpanEvents::NONE;
        for part in trimmed.split(',') {
            let part = part.trim();
            match part {
                "new" => result |= SpanEvents::NEW,
                "close" => result |= SpanEvents::CLOSE,
                "active" => result |= SpanEvents::ACTIVE,
                other => return Err(format!("unknown span event: '{other}' (expected new, close, active, none, or all)")),
            }
        }
        Ok(result)
    }
}

impl std::fmt::Display for SpanEvents {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if *self == SpanEvents::NONE {
            return write!(f, "none");
        }
        if *self == SpanEvents::ALL {
            return write!(f, "all");
        }
        let mut parts = Vec::new();
        if self.contains(SpanEvents::NEW) { parts.push("new"); }
        if self.contains(SpanEvents::CLOSE) { parts.push("close"); }
        if self.contains(SpanEvents::ACTIVE) { parts.push("active"); }
        write!(f, "{}", parts.join(","))
    }
}

impl SpanEvents {
    /// Convert to `tracing_subscriber::fmt::format::FmtSpan`.
    pub fn to_fmt_span(self) -> tracing_subscriber::fmt::format::FmtSpan {
        use tracing_subscriber::fmt::format::FmtSpan;
        let mut result = FmtSpan::NONE;
        if self.contains(SpanEvents::NEW) { result |= FmtSpan::NEW; }
        if self.contains(SpanEvents::CLOSE) { result |= FmtSpan::CLOSE; }
        if self.contains(SpanEvents::ACTIVE) { result |= FmtSpan::ACTIVE; }
        result
    }
}
```

- [ ] **Step 4: Wire up the module**

Add to `src/lib.rs` (after `mod config;`):
```rust
pub mod types;
```

Add to `src/tests/mod.rs`:
```rust
mod types_tests;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib types_tests`
Expected: all pass

- [ ] **Step 6: Run with otel feature for Transport tests**

Run: `cargo test --lib types_tests --features otel`
Expected: all pass including transport tests

- [ ] **Step 7: Commit**

```bash
git add src/types.rs src/tests/types_tests.rs src/tests/mod.rs src/lib.rs
git commit -m "feat: add Format, Transport, and SpanEvents types"
```

---

## Task 3: Create DestinationConfig and Per-Destination Settings

**Files:**
- Create: `src/dest_config.rs`
- Create: `src/tests/dest_config_tests.rs`
- Modify: `src/tests/mod.rs`
- Modify: `src/lib.rs` (add `mod dest_config`)

- [ ] **Step 1: Write tests for DestinationConfig resolution**

Create `src/tests/dest_config_tests.rs`:

```rust
use crate::dest_config::DestinationSettings;
use crate::types::{Format, SpanEvents};
use tracing::Level;

#[test]
fn test_wildcard_level_applies_to_all() {
    let mut settings = DestinationSettings::new();
    settings.set_level("*", Level::INFO);

    assert_eq!(settings.resolve_level("console"), Some(Level::INFO));
    assert_eq!(settings.resolve_level("file"), Some(Level::INFO));
    assert_eq!(settings.resolve_level("gelf"), Some(Level::INFO));
    assert_eq!(settings.resolve_level("otel"), Some(Level::INFO));
}

#[test]
fn test_specific_overrides_wildcard() {
    let mut settings = DestinationSettings::new();
    settings.set_level("*", Level::INFO);
    settings.set_level("console", Level::DEBUG);

    assert_eq!(settings.resolve_level("console"), Some(Level::DEBUG));
    assert_eq!(settings.resolve_level("file"), Some(Level::INFO));
}

#[test]
fn test_wildcard_format_applies_to_all() {
    let mut settings = DestinationSettings::new();
    settings.set_format("*", Format::Json);

    assert_eq!(settings.resolve_format("console"), Some(Format::Json));
    assert_eq!(settings.resolve_format("file"), Some(Format::Json));
}

#[test]
fn test_specific_format_overrides_wildcard() {
    let mut settings = DestinationSettings::new();
    settings.set_format("*", Format::Full);
    settings.set_format("console", Format::Pretty);

    assert_eq!(settings.resolve_format("console"), Some(Format::Pretty));
    assert_eq!(settings.resolve_format("file"), Some(Format::Full));
}

#[test]
fn test_filter_resolution() {
    let mut settings = DestinationSettings::new();
    settings.set_filter("*", "my_crate=info");
    settings.set_filter("console", "my_crate=debug,tower=warn");

    assert_eq!(settings.resolve_filter("console"), Some("my_crate=debug,tower=warn"));
    assert_eq!(settings.resolve_filter("file"), Some("my_crate=info"));
}

#[test]
fn test_no_settings_returns_none() {
    let settings = DestinationSettings::new();
    assert_eq!(settings.resolve_level("console"), None);
    assert_eq!(settings.resolve_format("console"), None);
    assert_eq!(settings.resolve_filter("console"), None);
}

#[test]
fn test_bool_settings() {
    let mut settings = DestinationSettings::new();
    settings.set_ansi("console", true);
    settings.set_timestamps("*", true);
    settings.set_timestamps("file", false);

    assert_eq!(settings.resolve_ansi("console"), Some(true));
    assert_eq!(settings.resolve_timestamps("console"), Some(true));
    assert_eq!(settings.resolve_timestamps("file"), Some(false));
}

#[test]
fn test_span_events() {
    let mut settings = DestinationSettings::new();
    settings.set_span_events("console", SpanEvents::NEW | SpanEvents::CLOSE);

    assert_eq!(settings.resolve_span_events("console"), Some(SpanEvents::NEW | SpanEvents::CLOSE));
    assert_eq!(settings.resolve_span_events("file"), None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib dest_config_tests`
Expected: compilation error — `dest_config` module doesn't exist

- [ ] **Step 3: Create `src/dest_config.rs`**

```rust
//! Per-destination settings resolution.
//!
//! Stores settings keyed by destination name (`"console"`, `"file"`, `"gelf"`, `"otel"`)
//! or `"*"` for defaults. Specific destination settings override wildcard defaults.

use std::collections::HashMap;
use tracing::Level;

use crate::types::{Format, SpanEvents};

/// Holds per-destination settings set via the builder API.
///
/// Each setting can be set for `"*"` (all destinations) or a specific destination.
/// `resolve_*` methods return the most specific value: destination-specific if set,
/// otherwise wildcard, otherwise `None`.
#[derive(Debug, Clone, Default)]
pub struct DestinationSettings {
    levels: HashMap<String, Level>,
    filters: HashMap<String, String>,
    formats: HashMap<String, Format>,
    ansi: HashMap<String, bool>,
    timestamps: HashMap<String, bool>,
    target: HashMap<String, bool>,
    thread_names: HashMap<String, bool>,
    file_line: HashMap<String, bool>,
    span_events: HashMap<String, SpanEvents>,
}

impl DestinationSettings {
    pub fn new() -> Self {
        Self::default()
    }

    // -- Setters --

    pub fn set_level(&mut self, dest: &str, level: Level) {
        self.levels.insert(dest.to_string(), level);
    }

    pub fn set_filter(&mut self, dest: &str, filter: &str) {
        self.filters.insert(dest.to_string(), filter.to_string());
    }

    pub fn set_format(&mut self, dest: &str, format: Format) {
        self.formats.insert(dest.to_string(), format);
    }

    pub fn set_ansi(&mut self, dest: &str, value: bool) {
        self.ansi.insert(dest.to_string(), value);
    }

    pub fn set_timestamps(&mut self, dest: &str, value: bool) {
        self.timestamps.insert(dest.to_string(), value);
    }

    pub fn set_target(&mut self, dest: &str, value: bool) {
        self.target.insert(dest.to_string(), value);
    }

    pub fn set_thread_names(&mut self, dest: &str, value: bool) {
        self.thread_names.insert(dest.to_string(), value);
    }

    pub fn set_file_line(&mut self, dest: &str, value: bool) {
        self.file_line.insert(dest.to_string(), value);
    }

    pub fn set_span_events(&mut self, dest: &str, events: SpanEvents) {
        self.span_events.insert(dest.to_string(), events);
    }

    // -- Resolvers (specific overrides wildcard) --

    pub fn resolve_level(&self, dest: &str) -> Option<Level> {
        self.levels.get(dest).or_else(|| self.levels.get("*")).copied()
    }

    pub fn resolve_filter(&self, dest: &str) -> Option<&str> {
        self.filters.get(dest).or_else(|| self.filters.get("*")).map(|s| s.as_str())
    }

    pub fn resolve_format(&self, dest: &str) -> Option<Format> {
        self.formats.get(dest).or_else(|| self.formats.get("*")).copied()
    }

    pub fn resolve_ansi(&self, dest: &str) -> Option<bool> {
        self.ansi.get(dest).or_else(|| self.ansi.get("*")).copied()
    }

    pub fn resolve_timestamps(&self, dest: &str) -> Option<bool> {
        self.timestamps.get(dest).or_else(|| self.timestamps.get("*")).copied()
    }

    pub fn resolve_target(&self, dest: &str) -> Option<bool> {
        self.target.get(dest).or_else(|| self.target.get("*")).copied()
    }

    pub fn resolve_thread_names(&self, dest: &str) -> Option<bool> {
        self.thread_names.get(dest).or_else(|| self.thread_names.get("*")).copied()
    }

    pub fn resolve_file_line(&self, dest: &str) -> Option<bool> {
        self.file_line.get(dest).or_else(|| self.file_line.get("*")).copied()
    }

    pub fn resolve_span_events(&self, dest: &str) -> Option<SpanEvents> {
        self.span_events.get(dest).or_else(|| self.span_events.get("*")).copied()
    }
}
```

- [ ] **Step 4: Wire up the module**

Add to `src/lib.rs` (after `pub mod types;`):
```rust
pub mod dest_config;
```

Add to `src/tests/mod.rs`:
```rust
mod dest_config_tests;
```

- [ ] **Step 5: Run tests**

Run: `cargo test --lib dest_config_tests`
Expected: all pass

- [ ] **Step 6: Commit**

```bash
git add src/dest_config.rs src/tests/dest_config_tests.rs src/tests/mod.rs src/lib.rs
git commit -m "feat: add DestinationSettings for per-destination config resolution"
```

---

## Task 4: Create TracingGuard

**Files:**
- Create: `src/guard.rs`
- Create: `src/tests/guard_tests.rs`
- Modify: `src/tests/mod.rs`
- Modify: `src/lib.rs` (add `mod guard`)

- [ ] **Step 1: Write tests for TracingGuard summary**

Create `src/tests/guard_tests.rs`:

```rust
use crate::guard::TracingGuard;

#[test]
fn test_summary_console_only() {
    let guard = TracingGuard::new(
        "console (full, INFO)".to_string(),
        None,
        None,
        None,
    );
    assert_eq!(guard.summary(), "console (full, INFO)");
}

#[test]
fn test_display_delegates_to_summary() {
    let guard = TracingGuard::new(
        "console (full, INFO)".to_string(),
        None,
        None,
        None,
    );
    assert_eq!(format!("{guard}"), "console (full, INFO)");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib guard_tests`
Expected: compilation error — `guard` module doesn't exist

- [ ] **Step 3: Create `src/guard.rs`**

**IMPORTANT:** `#[cfg]` on function parameters is NOT stable Rust. Do NOT use cfg attributes on constructor parameters. Instead, `TracingGuard` is constructed directly via struct literal by `init()` (which knows which features are enabled), and a `summary_only()` constructor is provided for testing.

```rust
//! Guard returned by [`TracingInit::init()`] that holds resources and flushes on drop.

use std::fmt;

/// Holds logging resources that must live for the application lifetime.
///
/// When dropped, performs orderly shutdown:
/// 1. Flush and shut down OTel TracerProvider (if active)
/// 2. Flush and shut down OTel LoggerProvider (if active)
/// 3. Drop the file appender WorkerGuard (flushes buffered writes)
///
/// Constructed directly via struct literal in `init()`. Use `summary_only()` for testing.
pub(crate) struct TracingGuard {
    pub(crate) summary_text: String,
    #[cfg(feature = "file")]
    pub(crate) _file_guard: Option<tracing_appender::non_blocking::WorkerGuard>,
    #[cfg(feature = "otel")]
    pub(crate) tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    #[cfg(feature = "otel")]
    pub(crate) logger_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
}

impl TracingGuard {
    /// Create a guard with only a summary string (for testing).
    #[cfg(test)]
    pub(crate) fn summary_only(summary: String) -> Self {
        TracingGuard {
            summary_text: summary,
            #[cfg(feature = "file")]
            _file_guard: None,
            #[cfg(feature = "otel")]
            tracer_provider: None,
            #[cfg(feature = "otel")]
            logger_provider: None,
        }
    }

    /// Returns a human-readable summary of the active logging setup.
    pub fn summary(&self) -> &str {
        &self.summary_text
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        #[cfg(feature = "otel")]
        {
            if let Some(ref provider) = self.tracer_provider {
                let _ = provider.shutdown();
            }
            if let Some(ref provider) = self.logger_provider {
                let _ = provider.shutdown();
            }
        }
        // File guard dropped automatically after OTel shutdown
    }
}

impl fmt::Display for TracingGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.summary_text)
    }
}

impl fmt::Debug for TracingGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TracingGuard")
            .field("summary", &self.summary_text)
            .finish()
    }
}
```

In `init()`, construct via struct literal:
```rust
let guard = TracingGuard {
    summary_text: self.build_summary(),
    #[cfg(feature = "file")]
    _file_guard: file_guard,
    #[cfg(feature = "otel")]
    tracer_provider,
    #[cfg(feature = "otel")]
    logger_provider,
};
```

- [ ] **Step 4: Wire up the module**

Add to `src/lib.rs`:
```rust
mod guard;
pub use guard::TracingGuard;
```

Add to `src/tests/mod.rs`:
```rust
mod guard_tests;
```

- [ ] **Step 5: Write guard tests using `summary_only()`**

```rust
use crate::guard::TracingGuard;

#[test]
fn test_summary_console_only() {
    let guard = TracingGuard::summary_only("console (full, INFO)".to_string());
    assert_eq!(guard.summary(), "console (full, INFO)");
}

#[test]
fn test_display_delegates_to_summary() {
    let guard = TracingGuard::summary_only("console (full, INFO)".to_string());
    assert_eq!(format!("{guard}"), "console (full, INFO)");
}
```

This works regardless of which features are enabled.

- [ ] **Step 6: Run tests**

Run: `cargo test --lib guard_tests`
Expected: all pass

- [ ] **Step 7: Commit**

```bash
git add src/guard.rs src/tests/guard_tests.rs src/tests/mod.rs src/lib.rs
git commit -m "feat: add TracingGuard with summary and flush-on-drop"
```

---

## Task 5: Rewrite Config Parsing for Nested TOML

**Files:**
- Modify: `src/config.rs`
- Modify: `src/tests/config_tests.rs`

This is the largest task. The config module is rewritten to parse nested TOML sections (`[logging.console]`, `[logging.file]`, `[logging.gelf]`, `[logging.otel]`) and resolve the 4-level inheritance chain.

- [ ] **Step 1: Write tests for nested config parsing**

Replace `src/tests/config_tests.rs` with tests for the new format. Key tests:

```rust
use crate::config::{apply_destination_modifier, LoggingConfig};

// --- Destination modifier tests (kept, updated s->g) ---

#[test]
fn test_absolute_destination() {
    assert_eq!(apply_destination_modifier(Some("cgf"), "cg"), "cg");
}

#[test]
fn test_remove_modifier() {
    assert_eq!(apply_destination_modifier(Some("cgf"), "-f"), "cg");
}

#[test]
fn test_add_modifier() {
    assert_eq!(apply_destination_modifier(Some("c"), "+g"), "cg");
}

#[test]
fn test_combined_modifier() {
    assert_eq!(apply_destination_modifier(Some("cgf"), "-f+o"), "cgo");
}

#[test]
fn test_add_otel_modifier() {
    assert_eq!(apply_destination_modifier(Some("cg"), "+o"), "cgo");
}

// --- Nested config parsing tests ---

#[test]
fn test_parse_nested_base() {
    let toml_str = r#"
[logging]
destination = "cg"
level = "info"
service_name = "my-service"
filter = "my_crate=debug"

[logging.console]
level = "debug"
format = "pretty"
ansi = true

[logging.file]
path = "logs"
prefix = "myapp"
rotation = "d:3"
format = "json"

[logging.gelf]
address = "localhost:12201"
level = "warn"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.destination.as_deref(), Some("cg"));
    assert_eq!(config.level.as_deref(), Some("info"));
    assert_eq!(config.service_name.as_deref(), Some("my-service"));
    assert_eq!(config.filter.as_deref(), Some("my_crate=debug"));

    let console = config.console.unwrap();
    assert_eq!(console.level.as_deref(), Some("debug"));
    assert_eq!(console.format.as_deref(), Some("pretty"));
    assert_eq!(console.ansi, Some(true));

    let file = config.file.unwrap();
    assert_eq!(file.path.as_deref(), Some("logs"));
    assert_eq!(file.prefix.as_deref(), Some("myapp"));
    assert_eq!(file.rotation.as_deref(), Some("d:3"));
    assert_eq!(file.format.as_deref(), Some("json"));

    let gelf = config.gelf.unwrap();
    assert_eq!(gelf.address.as_deref(), Some("localhost:12201"));
    assert_eq!(gelf.level.as_deref(), Some("warn"));
}

#[test]
fn test_parse_app_override_with_destination_sections() {
    let toml_str = r#"
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
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    // App overrides base level
    assert_eq!(config.level.as_deref(), Some("debug"));
    // App destination
    assert_eq!(config.destination.as_deref(), Some("co"));

    // myapp.console overrides console format
    let console = config.console.unwrap();
    assert_eq!(console.format.as_deref(), Some("json"));
    assert_eq!(console.level.as_deref(), Some("trace"));
}

#[test]
fn test_inheritance_chain() {
    // [logging.myapp.console] > [logging.myapp] > [logging.console] > [logging]
    let toml_str = r#"
[logging]
level = "info"
filter = "base_filter"

[logging.console]
format = "pretty"
filter = "console_filter"

[logging.myapp]
level = "debug"

[logging.myapp.console]
format = "json"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    let console = config.console.unwrap();
    // format from myapp.console (level 1)
    assert_eq!(console.format.as_deref(), Some("json"));
    // level from myapp (level 2) — not console's level
    // (console doesn't set level, myapp does)
    // filter from console (level 3)
    assert_eq!(console.filter.as_deref(), Some("console_filter"));
    // base level overridden by app
    assert_eq!(config.level.as_deref(), Some("debug"));
}

#[test]
fn test_otel_config() {
    let toml_str = r#"
[logging]
destination = "co"

[logging.otel]
endpoint = "http://localhost:4318"
transport = "http"
level = "error"

[logging.otel.resource]
"service.version" = "1.2.3"
"deployment.environment" = "staging"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    let otel = config.otel.unwrap();
    assert_eq!(otel.endpoint.as_deref(), Some("http://localhost:4318"));
    assert_eq!(otel.transport.as_deref(), Some("http"));
    assert_eq!(otel.level.as_deref(), Some("error"));

    let resource = otel.resource.unwrap();
    assert_eq!(resource.get("service.version").and_then(|v| v.as_str()), Some("1.2.3"));
    assert_eq!(resource.get("deployment.environment").and_then(|v| v.as_str()), Some("staging"));
}

#[test]
fn test_no_logging_section() {
    let toml_str = r#"
[other]
key = "value"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    assert!(LoggingConfig::from_toml(&value, "myapp").is_none());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib config_tests`
Expected: compilation errors — `LoggingConfig` doesn't have the new fields

- [ ] **Step 3: Rewrite `src/config.rs`**

Replace the `LoggingConfig` and `RawLoggingSection` structs with nested config types. Keep `apply_destination_modifier`, `find_file_upward`, `resolve_config_path`, `discover_config`, and `try_load_config` functions — update them for the new types.

New structs needed:

```rust
/// Console-specific config.
#[derive(Debug, Clone, Default)]
pub struct ConsoleConfig {
    pub level: Option<String>,
    pub filter: Option<String>,
    pub format: Option<String>,
    pub ansi: Option<bool>,
    pub timestamps: Option<bool>,
    pub target: Option<bool>,
    pub thread_names: Option<bool>,
    pub file_line: Option<bool>,
    pub span_events: Option<String>,
}

/// File-specific config.
#[derive(Debug, Clone, Default)]
pub struct FileConfig {
    pub level: Option<String>,
    pub filter: Option<String>,
    pub format: Option<String>,
    pub timestamps: Option<bool>,
    pub target: Option<bool>,
    pub thread_names: Option<bool>,
    pub file_line: Option<bool>,
    pub span_events: Option<String>,
    pub path: Option<String>,
    pub prefix: Option<String>,
    pub rotation: Option<String>,
}

/// GELF-specific config.
#[derive(Debug, Clone, Default)]
pub struct GelfConfig {
    pub level: Option<String>,
    pub filter: Option<String>,
    pub address: Option<String>,
}

/// OTel-specific config.
#[derive(Debug, Clone, Default)]
pub struct OtelConfig {
    pub level: Option<String>,
    pub filter: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub resource: Option<toml::value::Table>,
}

/// Top-level parsed config.
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    pub destination: Option<String>,
    pub level: Option<String>,
    pub filter: Option<String>,
    pub service_name: Option<String>,
    pub console: Option<ConsoleConfig>,
    pub file: Option<FileConfig>,
    pub gelf: Option<GelfConfig>,
    pub otel: Option<OtelConfig>,
}
```

The `from_toml` method must:
1. Parse `[logging]` base fields
2. Parse `[logging.console]`, `[logging.file]`, `[logging.gelf]`, `[logging.otel]` sections (skipping reserved names)
3. If `[logging.<app_name>]` exists, layer it on top of base
4. If `[logging.<app_name>.console]` etc. exist, layer them using the 4-level inheritance chain

Reserved section names: `console`, `file`, `gelf`, `otel`. Within `otel`, `resource` is reserved.

The implementer should write the full parsing logic including the inheritance chain resolution. The key invariant: for each field in a destination config, check (in order): app.dest section → app section → dest section → base section. First non-None wins.

- [ ] **Step 4: Run tests**

Run: `cargo test --lib config_tests`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add src/config.rs src/tests/config_tests.rs
git commit -m "feat: rewrite config for nested TOML with per-destination settings"
```

---

## Task 6: Refactor Builder API and Layer Assembly

**Files:**
- Modify: `src/lib.rs`
- Modify: `src/tests/integration_tests.rs`

This refactors the `TracingInit` builder to use the new destination-keyed API, per-layer filtering, and return `TracingGuard`.

- [ ] **Step 1: Update integration tests for new API**

Replace `src/tests/integration_tests.rs`:

```rust
use crate::TracingInit;
use crate::types::Format;
use tracing::Level;

#[test]
fn test_config_file_and_config_toml_both_panics() {
    let toml_str = "[logging]\ndestination = \"c\"\n";
    let value: toml::Value = toml_str.parse().unwrap();

    let result = std::panic::catch_unwind(|| {
        let mut builder = TracingInit::builder("test");
        builder.config_file("test.toml").config_toml(&value);
    });

    assert!(result.is_err());
}

#[test]
fn test_builder_destination_keyed_api() {
    let mut builder = TracingInit::builder("testapp");
    builder
        .destination("cg")
        .level("*", Level::INFO)
        .level("console", Level::DEBUG)
        .format("console", Format::Pretty)
        .no_auto_config_file()
        .ignore_environment_variables();

    let debug = format!("{:?}", builder);
    assert!(debug.contains("testapp"));
}

#[test]
fn test_log_to_legacy_methods() {
    let mut builder = TracingInit::builder("testapp");
    builder
        .log_to_console(true)
        .log_to_file(true)
        .log_to_gelf_server(false)
        .no_auto_config_file()
        .ignore_environment_variables();

    let debug = format!("{:?}", builder);
    assert!(debug.contains("testapp"));
}

#[test]
#[ignore] // Global subscriber can only be set once per process
fn test_full_logging_new_api() {
    use tracing::{event, Level};

    let guard = TracingInit::builder("App")
        .destination("c")
        .level("*", Level::INFO)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .unwrap();

    println!("Logging: {guard}");
    event!(Level::INFO, "test with new API");

    // guard lives until end of test
    drop(guard);
}
```

- [ ] **Step 2: Refactor `src/lib.rs`**

Major changes:
1. Replace the flat `enable_console`, `enable_log_file`, `enable_log_server` fields with a destination string and `DestinationSettings`
2. Add `service_name` field
3. Replace `level` and `filter` with per-destination settings via `DestinationSettings`
4. Feature-gate `config_source`, GELF, file, and OTel code
5. Change `init()` to return `TracingGuard`
6. Use per-layer `EnvFilter` via `Layer::with_filter()`
7. Rename `log_to_server` to `log_to_gelf_server`
8. Add destination-keyed methods: `level(&str, Level)`, `filter(&str, &str)`, `format(&str, Format)`, etc.
9. Keep legacy methods: `log_to_console`, `log_to_file`, `ignore_environment_variables`, `no_auto_config_file`, `config_file`, `config_toml`
10. Build summary string with per-destination details

The builder struct becomes:

```rust
#[derive(Debug, Clone)]
pub struct TracingInit {
    app_name: String,
    service_name: Option<String>,
    destination: Option<String>,
    dest_settings: DestinationSettings,

    // Legacy enable flags (set by log_to_* methods)
    enable_console: Option<bool>,
    enable_file: Option<bool>,
    enable_gelf: Option<bool>,
    #[cfg(feature = "otel")]
    enable_otel: Option<bool>,

    // File-specific (only when feature = "file")
    #[cfg(feature = "file")]
    file_path: Option<String>,
    #[cfg(feature = "file")]
    file_prefix: String,
    #[cfg(feature = "file")]
    file_rotation: Option<String>,

    // GELF-specific (only when feature = "gelf")
    #[cfg(feature = "gelf")]
    gelf_address: Option<String>,

    // OTel-specific (only when feature = "otel")
    #[cfg(feature = "otel")]
    otel_endpoint: Option<String>,
    #[cfg(feature = "otel")]
    otel_transport: Option<crate::types::Transport>,
    #[cfg(feature = "otel")]
    otel_resource_attrs: Vec<(String, String)>,

    // Config
    #[cfg(feature = "config")]
    config_source: Option<ConfigSource>,
    #[cfg(feature = "config")]
    config_summary: Option<String>,
    no_auto_config_file: bool,
    ignore_env_vars: bool,
}
// NOTE: config_file() and config_toml() methods must be behind #[cfg(feature = "config")]
// since ConfigSource uses toml::Value. The integration test that calls these also needs
// #[cfg(feature = "config")].
```

Layer assembly changes from global filter to per-layer:

```rust
fn build_env_filter(&self, dest: &str) -> Result<EnvFilter, Box<dyn std::error::Error>> {
    // Check for destination-specific filter
    if let Some(filter) = self.dest_settings.resolve_filter(dest) {
        let level = self.dest_settings.resolve_level(dest)
            .unwrap_or(Level::INFO);
        return Ok(EnvFilter::builder()
            .with_default_directive(level.into())
            .parse(filter)?);
    }
    // Destination has its own level but no filter — use level only.
    // IMPORTANT: Do NOT call from_env_lossy() here. Per the spec, RUST_LOG
    // only overrides the base filter, not per-destination filters.
    if let Some(level) = self.dest_settings.resolve_level(dest) {
        return Ok(EnvFilter::new(level.to_string()));
    }
    // No destination-specific settings — use base level with RUST_LOG override.
    // This is the ONLY path where RUST_LOG applies.
    let base_level = self.dest_settings.resolve_level("*").unwrap_or(Level::INFO);
    Ok(EnvFilter::builder()
        .with_default_directive(base_level.into())
        .from_env_lossy())
}
```

**IMPORTANT: `BoxedLayer` type must use concrete `Registry`**, not generic `S`. Per-layer filtering via `.with_filter()` returns `Filtered<L, F, S>` which can only be boxed as `Box<dyn Layer<S>>` when `S` is concrete. Change the type alias:

```rust
use tracing_subscriber::Registry;
type BoxedLayer = Option<Box<dyn Layer<Registry> + Send + Sync + 'static>>;
```

All `get_*_layer` methods lose the generic `<S>` parameter and return `BoxedLayer` directly. Example for console:

```rust
fn get_console_layer(&self) -> Result<BoxedLayer, Box<dyn std::error::Error>> {
    if !self.is_dest_enabled('c') { return Ok(None); }

    let filter = self.build_env_filter("console")?;
    let format = self.dest_settings.resolve_format("console").unwrap_or(Format::Full);
    let ansi = self.dest_settings.resolve_ansi("console").unwrap_or(true);
    let target = self.dest_settings.resolve_target("console").unwrap_or(true);
    let thread_names = self.dest_settings.resolve_thread_names("console").unwrap_or(false);
    let file_line = self.dest_settings.resolve_file_line("console").unwrap_or(false);
    let span_events = self.dest_settings.resolve_span_events("console")
        .unwrap_or(SpanEvents::NONE).to_fmt_span();

    let layer = match format {
        Format::Pretty => tracing_subscriber::fmt::layer()
            .pretty()
            .with_ansi(ansi)
            .with_target(target)
            .with_thread_names(thread_names)
            .with_file(file_line)
            .with_line_number(file_line)
            .with_span_events(span_events)
            .with_writer(std::io::stdout)
            .boxed(),
        Format::Json => tracing_subscriber::fmt::layer()
            .json()
            .with_ansi(ansi)
            .with_target(target)
            .with_thread_names(thread_names)
            .with_file(file_line)
            .with_line_number(file_line)
            .with_span_events(span_events)
            .with_writer(std::io::stdout)
            .boxed(),
        Format::Compact => tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(ansi)
            .with_target(target)
            .with_thread_names(thread_names)
            .with_file(file_line)
            .with_line_number(file_line)
            .with_span_events(span_events)
            .with_writer(std::io::stdout)
            .boxed(),
        Format::Full => tracing_subscriber::fmt::layer()
            .with_ansi(ansi)
            .with_target(target)
            .with_thread_names(thread_names)
            .with_file(file_line)
            .with_line_number(file_line)
            .with_span_events(span_events)
            .with_writer(std::io::stdout)
            .boxed(),
    };

    Ok(Some(layer.with_filter(filter).boxed()))
}
```

The `init()` method assembles all layers with per-layer filters and returns `TracingGuard`.

- [ ] **Step 3: Run tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs src/tests/integration_tests.rs
git commit -m "feat: refactor builder for destination-keyed API and per-layer filtering"
```

---

## Task 7: GELF Enrichment with Span Context

**Files:**
- Modify: `src/gelf.rs`
- Create: `src/tests/gelf_enrichment_tests.rs`
- Modify: `src/tests/mod.rs`

- [ ] **Step 1: Write tests for GELF enrichment**

Create `src/tests/gelf_enrichment_tests.rs`:

```rust
use serde_json::{Map, Value};

/// Helper: simulate what the enriched GelfLayer would produce from span context.
/// This tests the field extraction logic independent of UDP sending.

#[test]
fn test_gelf_includes_service_name() {
    let mut fields = Map::new();
    crate::gelf::add_service_field(&mut fields, Some("my-service"));
    assert_eq!(fields.get("_service").and_then(|v| v.as_str()), Some("my-service"));
}

#[test]
fn test_gelf_includes_target() {
    let mut fields = Map::new();
    crate::gelf::add_metadata_fields(&mut fields, Some("my_crate::module"), Some("src/main.rs"), Some(42));
    assert_eq!(fields.get("_target").and_then(|v| v.as_str()), Some("my_crate::module"));
    assert_eq!(fields.get("_file").and_then(|v| v.as_str()), Some("src/main.rs"));
    assert_eq!(fields.get("_line").and_then(|v| v.as_u64()), Some(42));
}

#[test]
fn test_gelf_service_name_none() {
    let mut fields = Map::new();
    crate::gelf::add_service_field(&mut fields, None);
    assert!(!fields.contains_key("_service"));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib gelf_enrichment_tests`
Expected: compilation error — functions don't exist

- [ ] **Step 3: Enrich `src/gelf.rs`**

Add the following to `GelfLayer`:
1. `service_name: Option<String>` field
2. Update constructor to accept `service_name`
3. In `on_event`, extract current span context:
   - Span name from `ctx.current_span()` and metadata lookup
   - Span fields via `SpanRef::extensions()` and visiting stored fields
   - `_target` from event metadata `target()`
   - `_service` from stored `service_name`
4. Add `#[cfg(feature = "otel")]` block to extract trace_id/span_id from `tracing_opentelemetry::OtelData`
5. Extract helper functions `add_service_field` and `add_metadata_fields` as `pub(crate)` for testability

Updated `GelfLayer`:

```rust
pub struct GelfLayer {
    socket: UdpSocket,
    addr: SocketAddr,
    base_fields: Map<String, Value>,
    service_name: Option<String>,
}

impl GelfLayer {
    pub fn new(
        addr: &str,
        additional_fields: Vec<(&str, String)>,
        service_name: Option<String>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // ... existing setup code ...
        Ok(GelfLayer {
            socket,
            addr: resolved,
            base_fields,
            service_name,
        })
    }
}
```

In `on_event`:

**IMPORTANT:** The existing code already inserts `_file`, `_line`, and `_module_path`. Replace those existing insertions with the new helper to avoid duplication. Do NOT add `add_metadata_fields` alongside the existing metadata code — replace it.

```rust
fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
    let mut fields = self.base_fields.clone();

    // ... existing level, _level code ...

    // Metadata fields — REPLACES the existing _file, _line, _module_path insertions
    add_metadata_fields(&mut fields,
        Some(event.metadata().target()),
        event.metadata().file(),
        event.metadata().line());

    // Service name
    add_service_field(&mut fields, self.service_name.as_deref());

    // Current span context
    if let Some(span) = ctx.lookup_current() {
        fields.insert("_span_name".into(), json!(span.name()));

        // Visit span fields and add as _span_* fields
        let extensions = span.extensions();
        if let Some(span_fields) = extensions.get::<SpanFields>() {
            for (key, value) in &span_fields.fields {
                fields.insert(format!("_span_{key}"), value.clone());
            }
        }

        // OTel trace context (only with otel feature)
        #[cfg(feature = "otel")]
        {
            use opentelemetry::trace::TraceContextExt;
            if let Some(otel_data) = extensions.get::<tracing_opentelemetry::OtelData>() {
                if let Some(parent_cx) = &otel_data.parent_cx {
                    let span_ctx = parent_cx.span().span_context().clone();
                    if span_ctx.is_valid() {
                        fields.insert("_trace_id".into(),
                            json!(format!("{:032x}", span_ctx.trace_id())));
                        fields.insert("_span_id".into(),
                            json!(format!("{:016x}", span_ctx.span_id())));
                    }
                }
            }
        }
    }

    // ... existing field visitor and send code ...
}
```

The `GelfLayer` also needs to implement `on_new_span` to store span fields in extensions for later retrieval. Add a `SpanFields` struct stored in span extensions:

```rust
#[derive(Debug)]
struct SpanFields {
    fields: Vec<(String, Value)>,
}

impl<S> Layer<S> for GelfLayer
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &tracing::span::Id, ctx: Context<'_, S>) {
        if let Some(span) = ctx.span(id) {
            let mut fields = Vec::new();
            let mut visitor = SpanFieldVisitor { fields: &mut fields };
            attrs.record(&mut visitor);
            span.extensions_mut().insert(SpanFields { fields });
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, S>) {
        // ... as above ...
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --lib gelf_enrichment_tests`
Expected: all pass

- [ ] **Step 5: Run all existing tests**

Run: `cargo test --lib`
Expected: all pass (existing GELF tests may need `service_name` parameter added to `GelfLayer::new`)

- [ ] **Step 6: Commit**

```bash
git add src/gelf.rs src/tests/gelf_enrichment_tests.rs src/tests/mod.rs
git commit -m "feat: enrich GELF with span context, trace IDs, and service name"
```

---

## Task 8: OpenTelemetry Module — Trace Export

**Files:**
- Create: `src/otel/mod.rs`
- Create: `src/otel/traces.rs`
- Modify: `src/lib.rs` (add `#[cfg(feature = "otel")] mod otel;`)

- [ ] **Step 1: Create `src/otel/mod.rs`**

```rust
//! OpenTelemetry OTLP export support (feature-gated behind `otel`).

pub mod traces;
pub mod logs;

use opentelemetry::KeyValue;
use opentelemetry_sdk::Resource;

/// Build an OTel Resource from service name and additional attributes.
pub fn build_resource(
    service_name: &str,
    extra_attrs: &[(String, String)],
) -> Resource {
    let mut attrs = vec![
        KeyValue::new("service.name", service_name.to_string()),
    ];
    for (key, value) in extra_attrs {
        attrs.push(KeyValue::new(key.clone(), value.clone()));
    }
    Resource::builder().with_attributes(attrs).build()
}
```

- [ ] **Step 2: Create `src/otel/traces.rs`**

```rust
//! OTel trace exporter layer construction.

use opentelemetry::trace::TracerProvider as _;  // Required for .tracer() method
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;

/// Create a TracerProvider with OTLP exporter.
///
/// Returns the provider (to be held in TracingGuard for shutdown)
/// and the tracing-opentelemetry layer.
pub fn create_trace_layer<S>(
    endpoint: &str,
    #[allow(unused_variables)]
    transport: &str,
    resource: Resource,
) -> Result<(SdkTracerProvider, tracing_opentelemetry::OpenTelemetryLayer<S, opentelemetry_sdk::trace::Tracer>), Box<dyn std::error::Error>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    let exporter = match transport {
        #[cfg(feature = "otel-grpc")]
        "grpc" => opentelemetry_otlp::SpanExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?,
        _ => opentelemetry_otlp::SpanExporter::builder()
            .with_http()
            .with_endpoint(format!("{endpoint}/v1/traces"))
            .build()?,
    };

    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let tracer = provider.tracer("tracing-init");
    let layer = tracing_opentelemetry::layer().with_tracer(tracer);

    Ok((provider, layer))
}
```

- [ ] **Step 3: Wire up in `src/lib.rs`**

Add after other module declarations:
```rust
#[cfg(feature = "otel")]
mod otel;
```

- [ ] **Step 4: Verify compilation**

Run: `cargo check --features otel`
Expected: compiles

- [ ] **Step 5: Commit**

```bash
git add src/otel/mod.rs src/otel/traces.rs src/lib.rs
git commit -m "feat: add OTel trace exporter layer"
```

---

## Task 9: OpenTelemetry Module — Log Export

**Files:**
- Create: `src/otel/logs.rs`

- [ ] **Step 1: Create `src/otel/logs.rs`**

```rust
//! OTel log exporter layer construction.

use opentelemetry_sdk::Resource;
use opentelemetry_sdk::logs::SdkLoggerProvider;

/// Create a LoggerProvider with OTLP exporter.
///
/// Returns the provider (to be held in TracingGuard for shutdown)
/// and the opentelemetry-appender-tracing layer.
pub fn create_log_layer(
    endpoint: &str,
    #[allow(unused_variables)]
    transport: &str,
    resource: Resource,
) -> Result<(SdkLoggerProvider, opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge<SdkLoggerProvider>), Box<dyn std::error::Error>>
{
    let exporter = match transport {
        #[cfg(feature = "otel-grpc")]
        "grpc" => opentelemetry_otlp::LogExporter::builder()
            .with_tonic()
            .with_endpoint(endpoint)
            .build()?,
        _ => opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_endpoint(format!("{endpoint}/v1/logs"))
            .build()?,
    };

    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(resource)
        .build();

    let layer = opentelemetry_appender_tracing::layer::OpenTelemetryTracingBridge::new(&provider);

    Ok((provider, layer))
}
```

- [ ] **Step 2: Verify compilation**

Run: `cargo check --features otel`
Expected: compiles

- [ ] **Step 3: Commit**

```bash
git add src/otel/logs.rs
git commit -m "feat: add OTel log exporter layer"
```

---

## Task 10: Wire OTel Layers into init()

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Add OTel layer assembly to `init()`**

In the `init()` method, after console/file/gelf layers:

```rust
#[cfg(feature = "otel")]
let (otel_trace_layer, otel_log_layer, tracer_provider, logger_provider) = if self.is_dest_enabled('o') {
    let endpoint = self.otel_endpoint.as_deref().unwrap_or("http://localhost:4318");
    let transport = self.otel_transport.map(|t| t.to_string()).unwrap_or_else(|| "http".to_string());
    let service = self.service_name.as_deref().unwrap_or(&self.app_name);
    let resource = otel::build_resource(service, &self.otel_resource_attrs);

    let (tp, trace_layer) = otel::traces::create_trace_layer(endpoint, &transport, resource.clone())?;
    let (lp, log_layer) = otel::logs::create_log_layer(endpoint, &transport, resource)?;

    let otel_filter = self.build_env_filter("otel")?;
    let otel_filter2 = self.build_env_filter("otel")?;

    (
        Some(trace_layer.with_filter(otel_filter).boxed()),
        Some(log_layer.with_filter(otel_filter2).boxed()),
        Some(tp),
        Some(lp),
    )
} else {
    (None, None, None, None)
};

#[cfg(not(feature = "otel"))]
let (otel_trace_layer, otel_log_layer): (BoxedLayer<_>, BoxedLayer<_>) = {
    if self.destination.as_ref().map_or(false, |d| d.contains('o')) {
        eprintln!("Warning: Destination 'o' requested but 'otel' feature not enabled — skipping");
    }
    (None, None)
};
```

Assembly:

```rust
tracing_subscriber::registry()
    .with(console_layer)
    .with(log_file_layer)
    .with(log_gelf_layer)
    .with(otel_trace_layer)
    .with(otel_log_layer)
    .init();
```

Return `TracingGuard` with all providers.

- [ ] **Step 2: Verify compilation with all features**

Run: `cargo check --all-features`
Expected: compiles

- [ ] **Step 3: Verify compilation without otel feature**

Run: `cargo check`
Expected: compiles

- [ ] **Step 4: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 5: Commit**

```bash
git add src/lib.rs
git commit -m "feat: wire OTel trace and log layers into init()"
```

---

## Task 11: Apply Config to Builder and Env Var Handling

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Update `apply_toml_config` for nested format**

The method needs to read the new nested `LoggingConfig` and populate `DestinationSettings` from the parsed sections. For each destination config section (console, file, gelf, otel), set the corresponding `dest_settings` values.

```rust
fn apply_toml_config(&mut self) {
    // ... existing config discovery logic ...
    let config = /* parsed LoggingConfig */;

    // Apply base settings
    if let Some(ref dest) = config.destination {
        if self.destination.is_none() { self.destination = Some(dest.clone()); }
    }
    if self.dest_settings.resolve_level("*").is_none() {
        if let Some(ref level_str) = config.level {
            if let Ok(level) = level_str.parse() {
                self.dest_settings.set_level("*", level);
            }
        }
    }
    if let Some(ref sn) = config.service_name {
        if self.service_name.is_none() { self.service_name = Some(sn.clone()); }
    }

    // Apply per-destination settings from TOML
    if let Some(ref console) = config.console {
        self.apply_dest_config_from_toml("console", console);
    }
    // ... same for file, gelf, otel ...
}
```

- [ ] **Step 2: Update `apply_environment_variables`**

Only handle the 4 retained env vars:

```rust
fn apply_environment_variables(&mut self) {
    if let Ok(dest) = std::env::var("LOG_DESTINATION") {
        if self.destination.is_none() {
            self.destination = Some(dest);
        }
    }
    if let Ok(level_str) = std::env::var("LOG_LEVEL") {
        if self.dest_settings.resolve_level("*").is_none() {
            if let Ok(level) = level_str.parse() {
                self.dest_settings.set_level("*", level);
            }
        }
    }
    // RUST_LOG is handled by EnvFilter::from_env_lossy() in build_env_filter
    // LOG_CONFIG is handled in apply_toml_config
}
```

- [ ] **Step 3: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 4: Commit**

```bash
git add src/lib.rs
git commit -m "feat: apply nested TOML config and env vars to builder"
```

---

## Task 12: Update Summary Output

**Files:**
- Modify: `src/lib.rs`

- [ ] **Step 1: Update `build_summary` for per-destination details**

```rust
fn build_summary(&self) -> String {
    let mut parts = Vec::new();

    if self.is_dest_enabled('c') {
        let format = self.dest_settings.resolve_format("console").unwrap_or(Format::Full);
        let level = self.dest_settings.resolve_level("console")
            .or_else(|| self.dest_settings.resolve_level("*"))
            .unwrap_or(Level::INFO);
        parts.push(format!("console ({format}, {level})"));
    }

    #[cfg(feature = "file")]
    if self.is_dest_enabled('f') {
        let format = self.dest_settings.resolve_format("file").unwrap_or(Format::Full);
        let level = self.dest_settings.resolve_level("file")
            .or_else(|| self.dest_settings.resolve_level("*"))
            .unwrap_or(Level::INFO);
        let path = self.file_path.as_deref().unwrap_or(".");
        let prefix = &self.file_prefix;
        parts.push(format!("file {path}/{prefix}.log ({format}, {level})"));
    }

    #[cfg(feature = "gelf")]
    if self.is_dest_enabled('g') {
        let level = self.dest_settings.resolve_level("gelf")
            .or_else(|| self.dest_settings.resolve_level("*"))
            .unwrap_or(Level::INFO);
        let addr = self.gelf_address.as_deref().unwrap_or("localhost:12201");
        parts.push(format!("gelf {addr} ({level})"));
    }

    #[cfg(feature = "otel")]
    if self.is_dest_enabled('o') {
        let level = self.dest_settings.resolve_level("otel")
            .or_else(|| self.dest_settings.resolve_level("*"))
            .unwrap_or(Level::INFO);
        let endpoint = self.otel_endpoint.as_deref().unwrap_or("http://localhost:4318");
        parts.push(format!("otel {endpoint} ({level})"));
    }

    let service = self.service_name.as_deref().unwrap_or(&self.app_name);
    parts.push(format!("service: {service}"));

    if let Some(ref config_source) = self.config_summary {
        parts.push(format!("config: {config_source}"));
    }

    parts.join(", ")
}
```

- [ ] **Step 2: Run all tests**

Run: `cargo test --lib`
Expected: all pass

- [ ] **Step 3: Commit**

```bash
git add src/lib.rs
git commit -m "feat: update summary with per-destination details"
```

---

## Task 13: Final Verification and Cleanup

**Files:**
- All modified files

- [ ] **Step 1: Run full test suite with default features**

Run: `cargo test`
Expected: all pass

- [ ] **Step 2: Run full test suite with all features**

Run: `cargo test --all-features`
Expected: all pass

- [ ] **Step 3: Run with no default features (core only)**

Run: `cargo check --no-default-features`
Expected: compiles (console-only logging)

- [ ] **Step 4: Run clippy**

Run: `cargo clippy --all-features -- -D warnings`
Expected: no warnings

- [ ] **Step 5: Run cargo doc**

Run: `cargo doc --all-features --no-deps`
Expected: docs build without warnings

- [ ] **Step 6: Commit any remaining fixes**

```bash
git add -A
git commit -m "chore: final cleanup and verification"
```

---

## Task Dependencies

Tasks MUST be executed sequentially in order. Key dependencies:
- Task 1 (Cargo.toml + cfg gates) must complete before any other task
- Tasks 2, 3, 4 are independent of each other but all depend on Task 1
- Task 5 (config rewrite) depends on Task 1
- Task 6 (builder refactor) depends on Tasks 2, 3, 4, and 5
- Task 7 (GELF enrichment) depends on Task 6 (for updated GelfLayer::new signature)
- Tasks 8, 9 (OTel modules) depend on Task 1
- Task 10 (wire OTel into init) depends on Tasks 6, 8, 9
- Tasks 11, 12 depend on Tasks 5, 6
- Task 13 depends on all previous tasks

## Notes for Implementer

### Key Implementation Details

1. **`BoxedLayer` type** must use concrete `Registry`, not generic `S`. Per-layer filters return `Filtered<L, F, S>` which requires a concrete subscriber type for boxing: `type BoxedLayer = Option<Box<dyn Layer<Registry> + Send + Sync>>;`

2. **Do NOT use `#[cfg]` on function parameters** — it's unstable. Use struct literal construction, conditional method calls, or the builder pattern instead.

3. **OTel trace/span ID extraction in GELF** requires looking up `tracing_opentelemetry::OtelData` in span extensions. This type is only available when the `otel` feature is enabled. Use `#[cfg(feature = "otel")]` blocks.

4. **The `tracing-opentelemetry` and `opentelemetry` crate versions must be compatible.** Check the compatibility matrix in `tracing-opentelemetry`'s docs. The versions in the spec (0.28/0.29) should be verified against the latest published versions at implementation time.

5. **`opentelemetry-appender-tracing`** bridges `tracing` log events to OTel log records. It creates an `OpenTelemetryTracingBridge` layer that captures events and forwards them as OTel logs. This is separate from the `tracing-opentelemetry` layer which handles spans/traces.

6. **Config feature gating**: `config_file()`, `config_toml()`, `ConfigSource`, `config_summary`, and all TOML-related code must be behind `#[cfg(feature = "config")]`. The builder should still work with just programmatic configuration.

7. **`RUST_LOG` must only apply to the base filter.** In `build_env_filter`, only call `from_env_lossy()` when a destination has NO explicit level or filter set (falling through to the `"*"` wildcard). Destinations with their own level should use `EnvFilter::new(level.to_string())` — no env var reading.

8. **GELF metadata deduplication**: The existing `on_event` code inserts `_file`, `_line`, and `_module_path`. When adding the enrichment, REPLACE these with the new `add_metadata_fields` helper — do not add alongside them or you get duplicate keys.

9. **`TracerProvider` trait import**: `provider.tracer("name")` requires `use opentelemetry::trace::TracerProvider;` — it's a trait method, not inherent.

//! TOML configuration parsing, layering, and destination modifier logic.
//!
//! This module handles loading a `[logging]` section from TOML configuration,
//! with support for per-application overrides and destination modifier syntax.
//! Config file discovery searches upward from the current working directory
//! for relative paths.

use std::path::{Path, PathBuf};

/// Apply a destination modifier to a base destination string.
///
/// If `modifier` starts with `+` or `-`, it modifies `base` by adding/removing
/// destination characters left-to-right. Otherwise it replaces `base` entirely.
///
/// # Examples
///
/// ```ignore
/// // Absolute replacement
/// assert_eq!(apply_destination_modifier(Some("csf"), "cf"), "cf");
///
/// // Add server, remove file
/// assert_eq!(apply_destination_modifier(Some("cf"), "+s-f"), "cs");
///
/// // Modifier on None base is a no-op
/// assert_eq!(apply_destination_modifier(None, "+s"), "s");
/// ```
pub fn apply_destination_modifier(base: Option<&str>, modifier: &str) -> String {
    let is_modifier = modifier.starts_with('+') || modifier.starts_with('-');

    if !is_modifier {
        return modifier.to_string();
    }

    let mut result: Vec<char> = base.unwrap_or("").chars().collect();
    let mut chars = modifier.chars().peekable();

    while let Some(op) = chars.next() {
        if let Some(&target) = chars.peek() {
            if target != '+' && target != '-' {
                chars.next();
                match op {
                    '-' => result.retain(|c| *c != target),
                    '+' => {
                        if !result.contains(&target) {
                            result.push(target);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    result.into_iter().collect()
}

/// Console output configuration parsed from `[logging.console]`.
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

/// File output configuration parsed from `[logging.file]`.
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

/// GELF server output configuration parsed from `[logging.gelf]`.
#[derive(Debug, Clone, Default)]
pub struct GelfConfig {
    pub level: Option<String>,
    pub filter: Option<String>,
    pub address: Option<String>,
}

/// OpenTelemetry output configuration parsed from `[logging.otel]`.
#[derive(Debug, Clone, Default)]
pub struct OtelConfig {
    pub level: Option<String>,
    pub filter: Option<String>,
    pub endpoint: Option<String>,
    pub transport: Option<String>,
    pub resource: Option<toml::value::Table>,
    /// Seconds between reprobe attempts when circuit is open (default 30).
    pub reprobe_interval: Option<u64>,
    /// Consecutive failures before opening the circuit (default 3).
    pub failure_threshold: Option<u32>,
    /// UDP multicast group for beacon listener (default "239.255.77.1").
    pub beacon_group: Option<String>,
    /// UDP port for beacon listener (default 4399).
    pub beacon_port: Option<u16>,
}

/// Parsed and layered logging configuration from nested TOML.
///
/// All fields are optional; `None` means "not configured" and the caller
/// should fall through to the next precedence level (environment variables,
/// then defaults).
///
/// Destination-specific settings live in sub-configs (`console`, `file`,
/// `gelf`, `otel`). The builder resolves the full inheritance chain at
/// runtime: destination config fields override top-level fields.
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    /// Destination flags: a string containing `c` (console), `f` (file),
    /// `g` (gelf), and/or `o` (otel).
    pub destination: Option<String>,
    /// Default log level name: `error`, `warn`, `info`, `debug`, or `trace`.
    pub level: Option<String>,
    /// An [`EnvFilter`](tracing_subscriber::EnvFilter) directive string.
    pub filter: Option<String>,
    /// Service name for telemetry.
    pub service_name: Option<String>,
    /// Console-specific settings.
    pub console: Option<ConsoleConfig>,
    /// File-specific settings.
    pub file: Option<FileConfig>,
    /// GELF-specific settings.
    pub gelf: Option<GelfConfig>,
    /// OpenTelemetry-specific settings.
    pub otel: Option<OtelConfig>,
}

/// Reserved section names that are destination configs, not app names.
const RESERVED_SECTIONS: &[&str] = &["console", "file", "gelf", "otel"];

impl LoggingConfig {
    /// Parse and layer config from a TOML value using the given app name.
    ///
    /// Returns `None` if there is no `[logging]` section.
    ///
    /// When a `[logging.<app_name>]` sub-table is present, its scalar fields
    /// override the base `[logging]` fields (destination uses modifier logic,
    /// others use first-non-None). Destination sub-sections are merged
    /// separately: `[logging.<app>.<dest>]` overrides `[logging.<dest>]`.
    pub fn from_toml(value: &toml::Value, app_name: &str) -> Option<LoggingConfig> {
        let logging = value.get("logging")?;
        let logging_table = logging.as_table()?;

        // Parse base scalar fields from [logging]
        let mut dest = Self::get_str(logging_table, "destination");
        let mut level = Self::get_str(logging_table, "level");
        let mut filter = Self::get_str(logging_table, "filter");
        let mut service_name = Self::get_str(logging_table, "service_name");

        // Parse base destination sections
        let base_console = Self::parse_console(logging_table.get("console"));
        let base_file = Self::parse_file(logging_table.get("file"));
        let base_gelf = Self::parse_gelf(logging_table.get("gelf"));
        let base_otel = Self::parse_otel(logging_table.get("otel"));

        // Check for app-specific section (skip reserved names)
        let app_table = if !RESERVED_SECTIONS.contains(&app_name) {
            logging_table.get(app_name).and_then(|v| v.as_table())
        } else {
            None
        };

        let (console, file, gelf, otel) = if let Some(app) = app_table {
            // Layer app base fields on top of logging base
            if let Some(app_dest) = Self::get_str(app, "destination") {
                let resolved = apply_destination_modifier(dest.as_deref(), &app_dest);
                dest = if resolved.is_empty() { None } else { Some(resolved) };
            }
            if let Some(v) = Self::get_str(app, "level") { level = Some(v); }
            if let Some(v) = Self::get_str(app, "filter") { filter = Some(v); }
            if let Some(v) = Self::get_str(app, "service_name") { service_name = Some(v); }

            // Parse app destination sections and layer over base
            let app_console = Self::parse_console(app.get("console"));
            let app_file = Self::parse_file(app.get("file"));
            let app_gelf = Self::parse_gelf(app.get("gelf"));
            let app_otel = Self::parse_otel(app.get("otel"));

            (
                Self::merge_console(base_console, app_console),
                Self::merge_file(base_file, app_file),
                Self::merge_gelf(base_gelf, app_gelf),
                Self::merge_otel(base_otel, app_otel),
            )
        } else {
            (base_console, base_file, base_gelf, base_otel)
        };

        Some(LoggingConfig {
            destination: dest,
            level,
            filter,
            service_name,
            console,
            file,
            gelf,
            otel,
        })
    }

    fn get_str(table: &toml::value::Table, key: &str) -> Option<String> {
        table.get(key).and_then(|v| v.as_str()).map(String::from)
    }

    fn get_bool(table: &toml::value::Table, key: &str) -> Option<bool> {
        table.get(key).and_then(|v| v.as_bool())
    }

    fn get_u64(table: &toml::value::Table, key: &str) -> Option<u64> {
        table.get(key).and_then(|v| v.as_integer()).map(|v| v as u64)
    }

    fn get_u32(table: &toml::value::Table, key: &str) -> Option<u32> {
        table.get(key).and_then(|v| v.as_integer()).map(|v| v as u32)
    }

    fn get_u16(table: &toml::value::Table, key: &str) -> Option<u16> {
        table.get(key).and_then(|v| v.as_integer()).map(|v| v as u16)
    }

    fn parse_console(value: Option<&toml::Value>) -> Option<ConsoleConfig> {
        let table = value?.as_table()?;
        Some(ConsoleConfig {
            level: Self::get_str(table, "level"),
            filter: Self::get_str(table, "filter"),
            format: Self::get_str(table, "format"),
            ansi: Self::get_bool(table, "ansi"),
            timestamps: Self::get_bool(table, "timestamps"),
            target: Self::get_bool(table, "target"),
            thread_names: Self::get_bool(table, "thread_names"),
            file_line: Self::get_bool(table, "file_line"),
            span_events: Self::get_str(table, "span_events"),
        })
    }

    fn parse_file(value: Option<&toml::Value>) -> Option<FileConfig> {
        let table = value?.as_table()?;
        Some(FileConfig {
            level: Self::get_str(table, "level"),
            filter: Self::get_str(table, "filter"),
            format: Self::get_str(table, "format"),
            timestamps: Self::get_bool(table, "timestamps"),
            target: Self::get_bool(table, "target"),
            thread_names: Self::get_bool(table, "thread_names"),
            file_line: Self::get_bool(table, "file_line"),
            span_events: Self::get_str(table, "span_events"),
            path: Self::get_str(table, "path"),
            prefix: Self::get_str(table, "prefix"),
            rotation: Self::get_str(table, "rotation"),
        })
    }

    fn parse_gelf(value: Option<&toml::Value>) -> Option<GelfConfig> {
        let table = value?.as_table()?;
        Some(GelfConfig {
            level: Self::get_str(table, "level"),
            filter: Self::get_str(table, "filter"),
            address: Self::get_str(table, "address"),
        })
    }

    fn parse_otel(value: Option<&toml::Value>) -> Option<OtelConfig> {
        let table = value?.as_table()?;
        let resource = table.get("resource").and_then(|v| v.as_table()).cloned();
        Some(OtelConfig {
            level: Self::get_str(table, "level"),
            filter: Self::get_str(table, "filter"),
            endpoint: Self::get_str(table, "endpoint"),
            transport: Self::get_str(table, "transport"),
            resource,
            reprobe_interval: Self::get_u64(table, "reprobe_interval"),
            failure_threshold: Self::get_u32(table, "failure_threshold"),
            beacon_group: Self::get_str(table, "beacon_group"),
            beacon_port: Self::get_u16(table, "beacon_port"),
        })
    }

    // Merge helpers: app.dest overrides dest (first non-None wins)

    fn merge_console(base: Option<ConsoleConfig>, app: Option<ConsoleConfig>) -> Option<ConsoleConfig> {
        match (base, app) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (Some(b), Some(a)) => Some(ConsoleConfig {
                level: a.level.or(b.level),
                filter: a.filter.or(b.filter),
                format: a.format.or(b.format),
                ansi: a.ansi.or(b.ansi),
                timestamps: a.timestamps.or(b.timestamps),
                target: a.target.or(b.target),
                thread_names: a.thread_names.or(b.thread_names),
                file_line: a.file_line.or(b.file_line),
                span_events: a.span_events.or(b.span_events),
            }),
        }
    }

    fn merge_file(base: Option<FileConfig>, app: Option<FileConfig>) -> Option<FileConfig> {
        match (base, app) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (Some(b), Some(a)) => Some(FileConfig {
                level: a.level.or(b.level),
                filter: a.filter.or(b.filter),
                format: a.format.or(b.format),
                timestamps: a.timestamps.or(b.timestamps),
                target: a.target.or(b.target),
                thread_names: a.thread_names.or(b.thread_names),
                file_line: a.file_line.or(b.file_line),
                span_events: a.span_events.or(b.span_events),
                path: a.path.or(b.path),
                prefix: a.prefix.or(b.prefix),
                rotation: a.rotation.or(b.rotation),
            }),
        }
    }

    fn merge_gelf(base: Option<GelfConfig>, app: Option<GelfConfig>) -> Option<GelfConfig> {
        match (base, app) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (Some(b), Some(a)) => Some(GelfConfig {
                level: a.level.or(b.level),
                filter: a.filter.or(b.filter),
                address: a.address.or(b.address),
            }),
        }
    }

    fn merge_otel(base: Option<OtelConfig>, app: Option<OtelConfig>) -> Option<OtelConfig> {
        match (base, app) {
            (None, None) => None,
            (Some(b), None) => Some(b),
            (None, Some(a)) => Some(a),
            (Some(b), Some(a)) => {
                // For resource tables, merge app entries over base
                let resource = match (b.resource, a.resource) {
                    (None, None) => None,
                    (Some(b_r), None) => Some(b_r),
                    (None, Some(a_r)) => Some(a_r),
                    (Some(mut b_r), Some(a_r)) => {
                        for (k, v) in a_r {
                            b_r.insert(k, v);
                        }
                        Some(b_r)
                    }
                };
                Some(OtelConfig {
                    level: a.level.or(b.level),
                    filter: a.filter.or(b.filter),
                    endpoint: a.endpoint.or(b.endpoint),
                    transport: a.transport.or(b.transport),
                    resource,
                    reprobe_interval: a.reprobe_interval.or(b.reprobe_interval),
                    failure_threshold: a.failure_threshold.or(b.failure_threshold),
                    beacon_group: a.beacon_group.or(b.beacon_group),
                    beacon_port: a.beacon_port.or(b.beacon_port),
                })
            }
        }
    }
}

/// Search upward from `start_dir` for a file with the given name.
/// Returns the full path if found, or `None`.
fn find_file_upward(filename: &str, start_dir: &Path) -> Option<PathBuf> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// Resolve a config file path. If the path is relative, search upward from CWD.
/// If absolute, use as-is.
fn resolve_config_path(path: &str) -> Option<PathBuf> {
    let file_path = Path::new(path);
    if file_path.is_absolute() {
        if file_path.is_file() { Some(file_path.to_path_buf()) } else { None }
    } else {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        find_file_upward(path, &cwd)
    }
}

/// Discover and load a logging configuration from a TOML file.
///
/// The discovery process:
/// 1. If `path` is provided, resolve it (with upward search for relative paths).
/// 2. If the file exists and contains a `[logging]` section, use it.
/// 3. If the file exists but has no `[logging]` section and `no_auto` is false,
///    try `logging.toml` in the same directory as a fallback.
/// 4. If the file doesn't exist and `no_auto` is false, try `logging.toml`
///    via upward search.
///
/// Returns the parsed config and a human-readable source description, or `None`.
pub fn discover_config(
    path: Option<&str>,
    app_name: &str,
    no_auto: bool,
) -> Option<(LoggingConfig, String)> {
    let path = path?;

    // Resolve the file path (with upward search for relative paths)
    if let Some(resolved) = resolve_config_path(path) {
        if let Some(result) = try_load_config(&resolved, app_name) {
            let source = format!("Config from {}", resolved.display());
            return Some((result, source));
        }

        // File found but no [logging] section -- fall back to logging.toml in same directory
        if !no_auto {
            let parent = resolved.parent().unwrap_or(Path::new("."));
            let fallback_path = parent.join("logging.toml");

            if let Some(result) = try_load_config(&fallback_path, app_name) {
                let source = format!("Config from {} (fallback)", fallback_path.display());
                return Some((result, source));
            }
        }
    } else if !no_auto {
        // File not found at all -- try logging.toml via upward search
        if let Some(resolved) = resolve_config_path("logging.toml") {
            if let Some(result) = try_load_config(&resolved, app_name) {
                let source = format!("Config from {} (fallback)", resolved.display());
                return Some((result, source));
            }
        }
    }

    None
}

/// Try to load a [`LoggingConfig`] from a specific file path.
///
/// Returns `None` if the file cannot be read, parsed, or has no `[logging]` section.
pub fn try_load_config(path: &Path, app_name: &str) -> Option<LoggingConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = content.parse().ok()?;
    LoggingConfig::from_toml(&value, app_name)
}

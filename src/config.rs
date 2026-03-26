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

/// Parsed and layered logging configuration from TOML.
///
/// All fields are optional; `None` means "not configured" and the caller
/// should fall through to the next precedence level (environment variables,
/// then defaults).
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    /// Destination flags: a string containing `c` (console), `f` (file), and/or `s` (server).
    pub destination: Option<String>,
    /// Log level name: `error`, `warn`, `info`, `debug`, or `trace`.
    pub level: Option<String>,
    /// An [`EnvFilter`](tracing_subscriber::EnvFilter) directive string.
    pub filter: Option<String>,
    /// GELF server address as `host:port`.
    pub server: Option<String>,
    /// Directory path for log files.
    pub file_path: Option<String>,
    /// Log file name prefix (overrides the app name default).
    pub file_prefix: Option<String>,
    /// Rotation spec: `d` (daily), `h` (hourly), `m` (minutely), `n` (never),
    /// optionally followed by `:<count>`.
    pub file_rotation: Option<String>,
}

/// Raw TOML section -- used for deserialization before layering.
/// Fields are extracted manually to tolerate unknown sub-table keys in `[logging]`.
#[derive(Debug, Clone, Default)]
struct RawLoggingSection {
    destination: Option<String>,
    level: Option<String>,
    filter: Option<String>,
    server: Option<String>,
    file_path: Option<String>,
    file_prefix: Option<String>,
    file_rotation: Option<String>,
}

impl LoggingConfig {
    /// Parse and layer config from a TOML value using the given app name.
    ///
    /// Returns `None` if there is no `[logging]` section.
    ///
    /// When a `[logging.<app_name>]` sub-table is present, its fields override
    /// the base `[logging]` fields with the following rules:
    /// - Most fields: app value wins; if absent, inherit from base; empty string clears.
    /// - `destination`: supports modifier syntax (`+s`, `-f`, `-f+s`).
    /// - `file_prefix`: never inherited from base -- the app must set it explicitly.
    pub fn from_toml(value: &toml::Value, app_name: &str) -> Option<LoggingConfig> {
        let logging = value.get("logging")?;
        let logging_table = logging.as_table()?;

        let base = Self::parse_section(logging)?;

        let app_section = logging_table
            .get(app_name)
            .and_then(Self::parse_section);

        if let Some(app) = app_section {
            Some(Self::layer(base, app))
        } else {
            Some(LoggingConfig {
                destination: base.destination,
                level: base.level,
                filter: base.filter,
                server: base.server,
                file_path: base.file_path,
                file_prefix: base.file_prefix,
                file_rotation: base.file_rotation,
            })
        }
    }

    /// Extract scalar string fields from a TOML table value, ignoring any sub-tables.
    fn parse_section(value: &toml::Value) -> Option<RawLoggingSection> {
        let table = value.as_table()?;
        let get_str = |key: &str| -> Option<String> {
            table.get(key).and_then(|v| v.as_str()).map(String::from)
        };
        Some(RawLoggingSection {
            destination: get_str("destination"),
            level: get_str("level"),
            filter: get_str("filter"),
            server: get_str("server"),
            file_path: get_str("file_path"),
            file_prefix: get_str("file_prefix"),
            file_rotation: get_str("file_rotation"),
        })
    }

    /// Layer an app section on top of a base section.
    fn layer(base: RawLoggingSection, app: RawLoggingSection) -> LoggingConfig {
        LoggingConfig {
            destination: Self::layer_destination(
                base.destination.as_deref(),
                app.destination.as_deref(),
            ),
            level: Self::layer_field(base.level, app.level),
            filter: Self::layer_field(base.filter, app.filter),
            server: Self::layer_field(base.server, app.server),
            file_path: Self::layer_field(base.file_path, app.file_path),
            file_prefix: Self::clear_empty(app.file_prefix),
            file_rotation: Self::layer_field(base.file_rotation, app.file_rotation),
        }
    }

    fn layer_destination(base: Option<&str>, app: Option<&str>) -> Option<String> {
        match app {
            Some(modifier) => {
                let result = apply_destination_modifier(base, modifier);
                if result.is_empty() {
                    None
                } else {
                    Some(result)
                }
            }
            None => base.map(String::from),
        }
    }

    fn layer_field(base: Option<String>, app: Option<String>) -> Option<String> {
        match app {
            Some(v) if v.is_empty() => None,
            Some(v) => Some(v),
            None => base,
        }
    }

    fn clear_empty(value: Option<String>) -> Option<String> {
        match value {
            Some(v) if v.is_empty() => None,
            other => other,
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

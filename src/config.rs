//! TOML configuration parsing, layering, and destination modifier logic.

/// Apply a destination modifier to a base destination string.
///
/// If `modifier` starts with `+` or `-`, it modifies `base` by adding/removing
/// characters left-to-right. Otherwise it replaces `base` entirely.
///
/// Applying a modifier when `base` is `None` is a no-op (returns empty string).
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
#[derive(Debug, Clone, Default)]
pub struct LoggingConfig {
    pub destination: Option<String>,
    pub level: Option<String>,
    pub filter: Option<String>,
    pub server: Option<String>,
    pub file_path: Option<String>,
    pub file_prefix: Option<String>,
    pub file_rotation: Option<String>,
}

/// Raw TOML section — used for deserialization before layering.
/// Fields are extracted manually to tolerate unknown sub-table keys in [logging].
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
    /// When a `[logging.<app_name>]` sub-table is present, its fields override
    /// the base `[logging]` fields. `file_prefix` is not inherited from base when
    /// a per-app section exists — the app must set it explicitly.
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
    ///
    /// Most fields: app value wins; if absent, inherit from base; empty string clears.
    /// `file_prefix`: never inherited from base — only set if app explicitly provides it.
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

use std::path::{Path, PathBuf};

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

        // File found but no [logging] section — fall back to logging.toml in same directory
        if !no_auto {
            let parent = resolved.parent().unwrap_or(Path::new("."));
            let fallback_path = parent.join("logging.toml");

            if let Some(result) = try_load_config(&fallback_path, app_name) {
                let source = format!("Config from {} (fallback)", fallback_path.display());
                return Some((result, source));
            }
        }
    } else if !no_auto {
        // File not found at all — try logging.toml via upward search
        if let Some(resolved) = resolve_config_path("logging.toml") {
            if let Some(result) = try_load_config(&resolved, app_name) {
                let source = format!("Config from {} (fallback)", resolved.display());
                return Some((result, source));
            }
        }
    }

    None
}

pub fn try_load_config(path: &Path, app_name: &str) -> Option<LoggingConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = content.parse().ok()?;
    LoggingConfig::from_toml(&value, app_name)
}

//! Simple tracing subscriber initialization with optional TOML configuration.
//!
//! `tracing-init` provides a builder-based API to set up [`tracing`] subscribers with
//! multiple output destinations: console, rotating log files, and a GELF-over-UDP server
//! (compatible with Graylog and other GELF consumers).
//!
//! Configuration is resolved from multiple sources with a clear precedence order,
//! making it easy to ship sensible defaults while allowing per-environment and
//! per-application overrides.
//!
//! # Quick Start
//!
//! ```no_run
//! // Minimal -- auto-discovers logging.toml if present
//! let summary = tracing_init::TracingInit::builder("myapp").init().unwrap();
//! println!("Logging: {summary}");
//! ```
//!
//! # TOML Configuration
//!
//! ```no_run
//! # #[cfg(feature = "config")]
//! # {
//! // Load from a config file (falls back to logging.toml if no [logging] section)
//! let summary = tracing_init::TracingInit::builder("myapp")
//!     .config_file("server.toml")
//!     .init()
//!     .unwrap();
//! # }
//! ```
//!
//! ## TOML Structure
//!
//! ```toml
//! [logging]
//! destination = "csf"          # c=console, f=file, s=server
//! level = "info"
//! server = "localhost:12201"
//! file_path = "logs"
//! file_rotation = "d:3"        # d=daily, h=hourly, m=minutely, n=never
//!
//! [logging.myapp]              # Per-app overrides
//! destination = "-f+s"         # Remove file, add server from base
//! level = "debug"
//! ```
//!
//! ## Destination Modifiers
//!
//! - Absolute: `"csf"` replaces the inherited value
//! - Modifier: `"-f"`, `"+s"`, `"-f+s"` adds/removes from inherited value
//!
//! ## Precedence (highest to lowest)
//!
//! 1. Explicit builder calls
//! 2. Environment variables (`LOG_DESTINATION`, `LOG_LEVEL`, `LOG_FILE_PATH`,
//!    `LOG_FILE_ROTATION`, `LOG_SERVER`, `RUST_LOG`)
//!    - `LOG_CONFIG` specifies a TOML config file path (used during auto-discovery)
//! 3. TOML config (app-specific over base)
//! 4. Defaults
//!
//! ## Environment Variables
//!
//! * `LOG_DESTINATION` — contains `c`, `f`, and/or `s`
//! * `LOG_FILE_PATH` — path to the log file directory
//! * `LOG_FILE_ROTATION` — `<rotation>[:<count>]` (d/h/m/n, default d:3)
//! * `LOG_SERVER` — GELF server address (host:port)
//! * `LOG_LEVEL` — error, warn, info, debug, trace
//! * `RUST_LOG` — filter directive
//! * `LOG_CONFIG` — path to a TOML config file (used during auto-discovery)
//!
use std::fmt::Display;

#[cfg(feature = "config")]
mod config;
#[cfg(feature = "gelf")]
mod gelf;

#[cfg(test)]
mod tests;

pub mod types;

use tracing::Level;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_subscriber::{EnvFilter, Layer};

/// Where the TOML configuration comes from — either a file path or a pre-parsed value.
#[cfg(feature = "config")]
#[derive(Debug, Clone)]
enum ConfigSource {
    /// A filesystem path to a TOML file.
    File(String),
    /// A pre-parsed [`toml::Value`] (useful when the caller already loaded the file).
    Toml(toml::Value),
}

/// Builder and initializer for a multi-destination [`tracing`] subscriber.
///
/// Create an instance with [`TracingInit::builder`], configure it with the chainable
/// setter methods, and finalize with [`TracingInit::init`]. Each setter records an
/// explicit value that takes the highest precedence; unset fields are filled from
/// environment variables, TOML config, and finally built-in defaults.
///
/// # Examples
///
/// ```no_run
/// # #[cfg(all(feature = "file", feature = "config"))]
/// # {
/// // Console + file, debug level, no TOML lookup
/// tracing_init::TracingInit::builder("myapp")
///     .log_to_console(true)
///     .log_to_file(true)
///     .level(tracing::Level::DEBUG)
///     .log_file_path("logs")
///     .no_auto_config_file()
///     .init()
///     .unwrap();
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct TracingInit {
    app_name: String,

    enable_console: Option<bool>,
    enable_log_file: Option<bool>,
    enable_log_server: Option<bool>,

    level: Option<Level>,

    #[cfg(feature = "file")]
    log_file_path: Option<String>,
    #[cfg(feature = "file")]
    log_file_prefix: String,
    #[cfg(feature = "file")]
    log_file_rotation: Option<tracing_appender::rolling::Rotation>,
    #[cfg(feature = "file")]
    log_file_backups: usize,

    #[cfg(feature = "gelf")]
    log_server_address: Option<String>,

    filter: Option<String>,

    #[cfg(feature = "config")]
    config_source: Option<ConfigSource>,
    #[cfg(feature = "config")]
    no_auto_config_file: bool,
    ignore_env_vars: bool,
    #[cfg(feature = "config")]
    config_summary: Option<String>,
}

type BoxedLayer<S> = Option<Box<dyn Layer<S> + Send + Sync + 'static>>;

impl TracingInit {
    /// Create a new builder with the given application name.
    ///
    /// The `app_name` is used as:
    /// - The GELF `_app` additional field when logging to a server.
    /// - The default log-file prefix (e.g. `myapp.2024-01-15.log`).
    /// - The key for per-app TOML overrides (`[logging.<app_name>]`).
    ///
    /// All output destinations start as *unset*; call the `log_to_*` methods or
    /// provide configuration via TOML / environment variables to enable them.
    pub fn builder(app_name: &str) -> TracingInit {
        TracingInit {
            app_name: app_name.to_string(),
            enable_console: None,
            enable_log_file: None,
            enable_log_server: None,

            // Default: INFO
            level: None,

            #[cfg(feature = "file")]
            log_file_path: None,
            #[cfg(feature = "file")]
            log_file_prefix: app_name.to_string(),

            // Default: tracing_appender::rolling::Rotation::DAILY
            #[cfg(feature = "file")]
            log_file_rotation: None,
            #[cfg(feature = "file")]
            log_file_backups: 3,

            // Default: "logging-server:12201"
            #[cfg(feature = "gelf")]
            log_server_address: None,

            filter: None,

            #[cfg(feature = "config")]
            config_source: None,
            #[cfg(feature = "config")]
            no_auto_config_file: false,
            ignore_env_vars: false,
            #[cfg(feature = "config")]
            config_summary: None,
        }
    }

    /// Enable or disable console (stdout) logging.
    ///
    /// When not called, the value is determined from `LOG_DESTINATION` (contains `c`)
    /// or the TOML `destination` field.
    pub fn log_to_console(&mut self, v: bool) -> &mut Self {
        self.enable_console = Some(v);
        self
    }

    /// Enable or disable rotating file logging.
    ///
    /// When not called, the value is determined from `LOG_DESTINATION` (contains `f`)
    /// or the TOML `destination` field. See [`log_file_path`](Self::log_file_path),
    /// [`log_file_rotation`](Self::log_file_rotation), and
    /// [`log_file_backups`](Self::log_file_backups) for related settings.
    #[cfg(feature = "file")]
    pub fn log_to_file(&mut self, v: bool) -> &mut Self {
        self.enable_log_file = Some(v);
        self
    }

    /// Enable or disable GELF-over-UDP server logging.
    ///
    /// When not called, the value is determined from `LOG_DESTINATION` (contains `s`)
    /// or the TOML `destination` field. The server address can be set with
    /// [`log_server_address`](Self::log_server_address).
    ///
    /// GELF messages are sent synchronously over a `std::net::UdpSocket`; no async
    /// runtime is required.
    #[cfg(feature = "gelf")]
    pub fn log_to_server(&mut self, v: bool) -> &mut Self {
        self.enable_log_server = Some(v);
        self
    }

    /// Set the default log level (default: `INFO`).
    ///
    /// This is used as the default directive for the [`EnvFilter`] when no explicit
    /// `RUST_LOG` or [`filter`](Self::filter) is provided. Can also be set via the
    /// `LOG_LEVEL` environment variable or the TOML `level` field.
    pub fn level(&mut self, level: Level) -> &mut Self {
        self.level = Some(level);
        self
    }

    /// Set an explicit [`EnvFilter`] directive string.
    ///
    /// When set, this overrides the default level and `RUST_LOG`. See the
    /// [`EnvFilter` documentation](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
    /// for the directive syntax.
    pub fn filter(&mut self, filter: &str) -> &mut Self {
        self.filter = Some(filter.to_string());
        self
    }

    /// Set the directory for log files (default: current directory).
    ///
    /// Can also be set via `LOG_FILE_PATH` or the TOML `file_path` field.
    #[cfg(feature = "file")]
    pub fn log_file_path(&mut self, path: &str) -> &mut Self {
        self.log_file_path = Some(path.to_string());
        self
    }

    /// Set the log file name prefix (default: the `app_name`).
    ///
    /// The final filename is `<prefix>.<date>.log` (or similar, depending on rotation).
    #[cfg(feature = "file")]
    pub fn log_file_prefix(&mut self, prefix: &str) -> &mut Self {
        self.log_file_prefix = prefix.to_string();
        self
    }

    /// Set the log file rotation policy (default: `DAILY`).
    ///
    /// Accepted values: [`Rotation::DAILY`], [`Rotation::HOURLY`],
    /// [`Rotation::MINUTELY`], [`Rotation::NEVER`].
    ///
    /// Can also be set via `LOG_FILE_ROTATION` (format: `d`, `h`, `m`, or `n`,
    /// optionally followed by `:<count>`) or the TOML `file_rotation` field.
    ///
    /// [`Rotation::DAILY`]: tracing_appender::rolling::Rotation::DAILY
    /// [`Rotation::HOURLY`]: tracing_appender::rolling::Rotation::HOURLY
    /// [`Rotation::MINUTELY`]: tracing_appender::rolling::Rotation::MINUTELY
    /// [`Rotation::NEVER`]: tracing_appender::rolling::Rotation::NEVER
    #[cfg(feature = "file")]
    pub fn log_file_rotation(
        &mut self,
        rotation: tracing_appender::rolling::Rotation,
    ) -> &mut Self {
        self.log_file_rotation = Some(rotation);
        self
    }

    /// Set the maximum number of rotated log files to keep (default: 3).
    ///
    /// Only meaningful when rotation is not [`Rotation::NEVER`].
    ///
    /// [`Rotation::NEVER`]: tracing_appender::rolling::Rotation::NEVER
    #[cfg(feature = "file")]
    pub fn log_file_backups(&mut self, backups: usize) -> &mut Self {
        self.log_file_backups = backups;
        self
    }

    /// Set the GELF server address as `host:port` (default: `"logging-server:12201"`).
    ///
    /// Can also be set via `LOG_SERVER` or the TOML `server` field. Using a DNS CNAME
    /// for `logging-server` is a convenient way to avoid hard-coding addresses.
    #[cfg(feature = "gelf")]
    pub fn log_server_address(&mut self, name: &str) -> &mut Self {
        self.log_server_address = Some(name.to_string());
        self
    }

    /// Provide a pre-parsed [`toml::Value`] as the configuration source.
    ///
    /// The value must contain a `[logging]` table. This is useful when the caller
    /// has already loaded a multi-purpose config file and wants to pass the parsed
    /// TOML directly.
    ///
    /// # Panics
    ///
    /// Panics if [`config_file`](Self::config_file) was already called.
    #[cfg(feature = "config")]
    pub fn config_toml(&mut self, value: &toml::Value) -> &mut Self {
        if self.config_source.is_some() {
            panic!("Cannot call both config_toml() and config_file()");
        }
        self.config_source = Some(ConfigSource::Toml(value.clone()));
        self
    }

    /// Set a TOML config file path to load logging configuration from.
    ///
    /// Relative paths are resolved by searching upward from the current working
    /// directory. If the file exists but has no `[logging]` section, the builder
    /// falls back to a `logging.toml` in the same directory (unless
    /// [`no_auto_config_file`](Self::no_auto_config_file) is set).
    ///
    /// # Panics
    ///
    /// Panics if [`config_toml`](Self::config_toml) was already called.
    #[cfg(feature = "config")]
    pub fn config_file(&mut self, path: &str) -> &mut Self {
        if self.config_source.is_some() {
            panic!("Cannot call both config_file() and config_toml()");
        }
        self.config_source = Some(ConfigSource::File(path.to_string()));
        self
    }

    /// Disable automatic discovery of `logging.toml`.
    ///
    /// By default, when no explicit config source is provided, the builder searches
    /// upward for a `logging.toml` file. Call this to suppress that behavior.
    #[cfg(feature = "config")]
    pub fn no_auto_config_file(&mut self) -> &mut Self {
        self.no_auto_config_file = true;
        self
    }

    /// Ignore all `LOG_*` and `RUST_LOG` environment variables.
    ///
    /// Useful for testing or when full programmatic control is desired.
    pub fn ignore_environment_variables(&mut self) -> &mut Self {
        self.ignore_env_vars = true;
        self
    }

    /// Resolve all configuration sources and install the global tracing subscriber.
    ///
    /// Returns a human-readable summary of the active configuration (destinations,
    /// level, config source) suitable for startup logging.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The log file directory cannot be created or written to.
    /// - The GELF server address cannot be resolved.
    /// - The filter directive string is invalid.
    pub fn init(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        if !self.ignore_env_vars {
            self.apply_environment_variables();
        }
        #[cfg(feature = "config")]
        self.apply_toml_config();
        self.apply_defaults();

        let console_layer = self.get_console_layer();
        #[cfg(feature = "file")]
        let log_file_layer = self.get_log_file_layer()?;
        #[cfg(not(feature = "file"))]
        let log_file_layer: BoxedLayer<_> = None;
        #[cfg(feature = "gelf")]
        let log_server_layer = self.get_log_server_layer()?;
        #[cfg(not(feature = "gelf"))]
        let log_server_layer: BoxedLayer<_> = None;

        let env_filter = if let Some(ref filter) = self.filter {
            EnvFilter::try_new(filter)?
        } else {
            EnvFilter::builder()
                .with_default_directive(self.level.unwrap().into())
                .from_env_lossy()
        };

        tracing_subscriber::registry()
            .with(console_layer)
            .with(log_file_layer)
            .with(log_server_layer)
            .with(env_filter)
            .init();

        Ok(self.build_summary())
    }

    fn apply_environment_variables(&mut self) {
        // Only apply LOG_DESTINATION when the env var is actually set,
        // so unset env vars don't block TOML config from filling in values.
        if let Ok(log_destination) = std::env::var("LOG_DESTINATION") {
            if self.enable_console.is_none() {
                self.enable_console = Some(log_destination.contains('c'));
            }
            if self.enable_log_file.is_none() {
                self.enable_log_file = Some(log_destination.contains('f'));
            }
            if self.enable_log_server.is_none() {
                self.enable_log_server = Some(log_destination.contains('s'));
            }
        }
        #[cfg(feature = "file")]
        if self.log_file_path.is_none() {
            if let Ok(path) = std::env::var("LOG_FILE_PATH") {
                self.log_file_path = Some(path);
            }
        }
        if self.level.is_none() {
            if let Ok(level_str) = std::env::var("LOG_LEVEL") {
                self.level = level_str.parse().ok();
            }
        }
        #[cfg(feature = "file")]
        if self.log_file_rotation.is_none() {
            if let Ok(rotation_value) = std::env::var("LOG_FILE_ROTATION") {
                let (rotation, count) = Self::parse_rotation_string(&rotation_value);
                self.log_file_rotation = Some(rotation);
                self.log_file_backups = count;
            }
        }
        #[cfg(feature = "gelf")]
        if self.log_server_address.is_none() {
            if let Ok(server) = std::env::var("LOG_SERVER") {
                self.log_server_address = Some(server);
            }
        }
    }

    #[cfg(feature = "config")]
    fn apply_toml_config(&mut self) {
        let (config, source) = match &self.config_source {
            Some(ConfigSource::Toml(value)) => {
                match config::LoggingConfig::from_toml(value, &self.app_name) {
                    Some(cfg) => (cfg, "Config from pre-parsed TOML".to_string()),
                    None => {
                        self.config_summary = Some("No [logging] section in provided TOML".to_string());
                        return;
                    }
                }
            }
            Some(ConfigSource::File(path)) => {
                match config::discover_config(Some(path), &self.app_name, self.no_auto_config_file) {
                    Some((cfg, source)) => (cfg, source),
                    None => {
                        self.config_summary = Some("No config file".to_string());
                        return;
                    }
                }
            }
            None => {
                if self.no_auto_config_file {
                    self.config_summary = Some("No config file".to_string());
                    return;
                }
                if let Ok(env_path) = std::env::var("LOG_CONFIG") {
                    match config::discover_config(Some(&env_path), &self.app_name, self.no_auto_config_file) {
                        Some((cfg, source)) => (cfg, format!("{} (via LOG_CONFIG)", source)),
                        None => {
                            self.config_summary = Some("No config file (LOG_CONFIG set but no [logging] found)".to_string());
                            return;
                        }
                    }
                } else {
                    match config::discover_config(Some("logging.toml"), &self.app_name, true) {
                        Some((cfg, source)) => (cfg, format!("{} (auto-discovered)", source)),
                        None => {
                            self.config_summary = Some("No config file".to_string());
                            return;
                        }
                    }
                }
            }
        };

        self.config_summary = Some(source);

        if let Some(dest) = &config.destination {
            if self.enable_console.is_none() { self.enable_console = Some(dest.contains('c')); }
            if self.enable_log_file.is_none() { self.enable_log_file = Some(dest.contains('f')); }
            if self.enable_log_server.is_none() { self.enable_log_server = Some(dest.contains('s')); }
        }
        if self.level.is_none() {
            if let Some(ref level_str) = config.level { self.level = level_str.parse().ok(); }
        }
        if self.filter.is_none() { self.filter = config.filter.clone(); }
        #[cfg(feature = "gelf")]
        if self.log_server_address.is_none() { self.log_server_address = config.server.clone(); }
        #[cfg(feature = "file")]
        {
            if self.log_file_path.is_none() { self.log_file_path = config.file_path.clone(); }
            if let Some(ref prefix) = config.file_prefix {
                if self.log_file_prefix == self.app_name { self.log_file_prefix = prefix.clone(); }
            }
            if self.log_file_rotation.is_none() {
                if let Some(ref rotation_str) = config.file_rotation {
                    let (rotation, count) = Self::parse_rotation_string(rotation_str);
                    self.log_file_rotation = Some(rotation);
                    self.log_file_backups = count;
                }
            }
        }
    }

    /// Parse a rotation string like `"d:3"` into a rotation policy and backup count.
    ///
    /// Format: `<letter>[:<count>]` where letter is `d` (daily), `h` (hourly),
    /// `m` (minutely), or `n` (never). Count defaults to 3.
    #[cfg(feature = "file")]
    fn parse_rotation_string(s: &str) -> (tracing_appender::rolling::Rotation, usize) {
        let mut parts = s.split(':');
        let rotation = parts.next().unwrap_or("d");
        let count = parts.next().and_then(|v| v.parse().ok()).unwrap_or(3);
        let rotation = match rotation {
            "d" => tracing_appender::rolling::Rotation::DAILY,
            "h" => tracing_appender::rolling::Rotation::HOURLY,
            "m" => tracing_appender::rolling::Rotation::MINUTELY,
            "n" => tracing_appender::rolling::Rotation::NEVER,
            _ => tracing_appender::rolling::Rotation::DAILY,
        };
        (rotation, count)
    }

    fn apply_defaults(&mut self) {
        if self.enable_console.is_none() { self.enable_console = Some(false); }
        if self.enable_log_file.is_none() { self.enable_log_file = Some(false); }
        if self.enable_log_server.is_none() { self.enable_log_server = Some(false); }
        if self.level.is_none() { self.level = Some(Level::INFO); }
        #[cfg(feature = "file")]
        {
            if self.log_file_path.is_none() { self.log_file_path = Some(String::new()); }
            if self.log_file_rotation.is_none() { self.log_file_rotation = Some(tracing_appender::rolling::Rotation::DAILY); }
        }
        #[cfg(feature = "gelf")]
        if self.log_server_address.is_none() { self.log_server_address = Some("logging-server:12201".to_string()); }
    }

    fn get_console_layer<S>(&self) -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
    where
        S: tracing::Subscriber,
        for<'a> S: LookupSpan<'a>,
    {
        if self.enable_console.unwrap_or(false) {
            Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(true)
                    .with_writer(std::io::stdout)
                    .boxed(),
            )
        } else {
            None
        }
    }

    #[cfg(feature = "file")]
    fn get_log_file_layer<S>(&self) -> Result<BoxedLayer<S>, Box<dyn std::error::Error>>
    where
        S: tracing::Subscriber,
        for<'a> S: LookupSpan<'a>,
    {
        if self.enable_log_file.unwrap_or(false) {
            let file_writer = tracing_appender::rolling::RollingFileAppender::builder()
                .filename_prefix(&self.log_file_prefix)
                .filename_suffix("log")
                .rotation(self.log_file_rotation.as_ref().unwrap().clone())
                .max_log_files(self.log_file_backups)
                .build(self.log_file_path.as_ref().unwrap())?;

            Ok(Some(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(file_writer)
                    .boxed(),
            ))
        } else {
            Ok(None)
        }
    }

    #[cfg(feature = "gelf")]
    fn get_log_server_layer<S>(&self) -> Result<BoxedLayer<S>, Box<dyn std::error::Error>>
    where
        S: tracing::Subscriber,
        for<'a> S: LookupSpan<'a>,
    {
        if self.enable_log_server.unwrap_or(false) {
            let addr = self.log_server_address.as_ref().unwrap();
            let layer = gelf::GelfLayer::new(
                addr,
                vec![("app", self.app_name.clone())],
            )?;
            Ok(Some(layer.boxed()))
        } else {
            Ok(None)
        }
    }
}

impl TracingInit {
    fn build_summary(&self) -> String {
        let mut parts = Vec::new();

        if self.enable_console.unwrap_or(false) {
            parts.push("log to console".to_string());
        }
        #[cfg(feature = "file")]
        if self.enable_log_file.unwrap_or(false) {
            let path = self.log_file_path.as_deref().unwrap_or(".");
            let path = if path.is_empty() { "." } else { path };
            let rotation = self.get_rotation_description();
            let mut file_part = format!("log to file {}/{}.log", path, self.log_file_prefix);
            if !rotation.is_empty() {
                file_part.push_str(&format!(", {}", rotation));
            }
            parts.push(file_part);
        }
        #[cfg(feature = "gelf")]
        if self.enable_log_server.unwrap_or(false) {
            parts.push(format!("log to server {}", self.log_server_address.as_deref().unwrap_or("unknown")));
        }

        let mut summary = parts.join(", ");
        if !summary.is_empty() {
            if let Some(level) = self.level {
                summary.push_str(&format!(", default level: {}", level));
            }
            if let Some(ref filter) = self.filter {
                summary.push_str(&format!(", ({})", filter));
            }
        }

        #[cfg(feature = "config")]
        if let Some(ref config_source) = self.config_summary {
            if summary.is_empty() {
                summary = config_source.clone();
            } else {
                summary.push_str(&format!(", {}", config_source));
            }
        }

        summary
    }

    #[cfg(feature = "file")]
    fn get_rotation_description(&self) -> String {
        if let Some(ref rotation) = self.log_file_rotation {
            let rotation_name = match *rotation {
                tracing_appender::rolling::Rotation::DAILY => "daily",
                tracing_appender::rolling::Rotation::HOURLY => "hourly",
                tracing_appender::rolling::Rotation::MINUTELY => "minutely",
                tracing_appender::rolling::Rotation::NEVER => "",
                _ => "other",
            };
            if *rotation != tracing_appender::rolling::Rotation::NEVER {
                format!("rotation: {}:{}", rotation_name, self.log_file_backups)
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    }
}

impl Display for TracingInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.build_summary())
    }
}

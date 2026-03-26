//! Simple tracing subscriber initialization with optional TOML configuration.
//!
//! # Quick Start
//!
//! ```no_run
//! // Minimal — auto-discovers logging.toml if present
//! let summary = tracing_init::TracingInit::builder("myapp").init().unwrap();
//! println!("Logging: {summary}");
//! ```
//!
//! # TOML Configuration
//!
//! ```no_run
//! // Load from a config file (falls back to logging.toml if no [logging] section)
//! let summary = tracing_init::TracingInit::builder("myapp")
//!     .config_file("server.toml")
//!     .init()
//!     .unwrap();
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

mod config;
mod gelf;

#[cfg(test)]
mod tests;

use tracing::Level;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_subscriber::{EnvFilter, Layer};

#[derive(Debug, Clone)]
enum ConfigSource {
    File(String),
    Toml(toml::Value),
}

/// Holds the configuration for the tracing subscriber
#[derive(Debug, Clone)]
pub struct TracingInit {
    app_name: String,

    enable_console: Option<bool>,
    enable_log_file: Option<bool>,
    enable_log_server: Option<bool>,

    level: Option<Level>,

    log_file_path: Option<String>,
    log_file_prefix: String,
    log_file_rotation: Option<tracing_appender::rolling::Rotation>,
    log_file_backups: usize,

    log_server_address: Option<String>,

    filter: Option<String>,

    config_source: Option<ConfigSource>,
    no_auto_config_file: bool,
    ignore_env_vars: bool,
    config_summary: Option<String>,
}

type BoxedLayer<S> = Option<Box<dyn Layer<S> + Send + Sync + 'static>>;

impl TracingInit {
    /// Create a new TraceInit with default values
    ///
    /// # Arguments
    ///    app_name - The application name. When sending logs to server, this name will be used as for the app field
    ///
    pub fn builder(app_name: &str) -> TracingInit {
        TracingInit {
            app_name: app_name.to_string(),
            enable_console: None,
            enable_log_file: None,
            enable_log_server: None,

            // Default: INFO
            level: None,

            log_file_path: None,
            log_file_prefix: app_name.to_string(),

            // Default: tracing_appender::rolling::Rotation::DAILY
            log_file_rotation: None,
            log_file_backups: 3,

            // Default: "logging-server:12201"
            log_server_address: None,

            filter: None,

            config_source: None,
            no_auto_config_file: false,
            ignore_env_vars: false,
            config_summary: None,
        }
    }

    /// determine if the console should be used for logging (default true if LOG_DESTINATION environment variable's value contains 'c' otherwise false)
    ///
    pub fn log_to_console(&mut self, v: bool) -> &mut Self {
        self.enable_console = Some(v);
        self
    }

    /// determine if the log file should be used for logging (default true if LOG_DESTINATION environment variable's value contains 'f' otherwise false)
    ///
    pub fn log_to_file(&mut self, v: bool) -> &mut Self {
        self.enable_log_file = Some(v);
        self
    }

    /// determine if the logs should be send using GELF protocol (default true if LOG_DESTINATION environment variable's value contains 's' otherwise false)
    ///
    /// # Notes
    /// Sending log to server works only if working under async runtime (e.g. tokio)
    ///
    pub fn log_to_server(&mut self, v: bool) -> &mut Self {
        self.enable_log_server = Some(v);
        self
    }

    /// Set the default log level (default: INFO)
    ///
    pub fn level(&mut self, level: Level) -> &mut Self {
        self.level = Some(level);
        self
    }

    /// Set the filter to use for the tracing subscriber (default: from environment variable RUST_LOG)
    /// Sett [filter syntax](https://docs.rs/tracing-subscriber/0.2.14/tracing_subscriber/filter/struct.EnvFilter.html#filter-syntax) for details
    pub fn filter(&mut self, filter: &str) -> &mut Self {
        self.filter = Some(filter.to_string());
        self
    }

    /// Set the path to the log file (default: current directory)
    ///
    pub fn log_file_path(&mut self, path: &str) -> &mut Self {
        self.log_file_path = Some(path.to_string());
        self
    }

    /// Set the default log file prefix (default: app name)
    ///
    pub fn log_file_prefix(&mut self, prefix: &str) -> &mut Self {
        self.log_file_prefix = prefix.to_string();
        self
    }

    /// Set the log file rotation (default: DAILY)
    ///
    /// # Notes
    ///  Th possible values are: DAILY, HOURLY, MINUTELY, NEVER
    ///
    pub fn log_file_rotation(
        &mut self,
        rotation: tracing_appender::rolling::Rotation,
    ) -> &mut Self {
        self.log_file_rotation = Some(rotation);
        self
    }

    /// Set the log file backups (default: 3)
    ///
    /// # Notes
    /// The number of log file backups is relevant only if the log file rotation is not set to NEVER
    ///
    pub fn log_file_backups(&mut self, backups: usize) -> &mut Self {
        self.log_file_backups = backups;
        self
    }

    /// Set the address of the logging server (default is the value of environment variable LOG_SERVER or "logging-server:12201" if the environment variable is not set)
    ///
    /// # Notes
    /// It is advisable to add CNAME record to your DNS to point logging-server to the actual logging server (or use LOGGING_SERVER environment variable)
    ///
    pub fn log_server_address(&mut self, name: &str) -> &mut Self {
        self.log_server_address = Some(name.to_string());
        self
    }

    pub fn config_toml(&mut self, value: &toml::Value) -> &mut Self {
        if self.config_source.is_some() {
            panic!("Cannot call both config_toml() and config_file()");
        }
        self.config_source = Some(ConfigSource::Toml(value.clone()));
        self
    }

    pub fn config_file(&mut self, path: &str) -> &mut Self {
        if self.config_source.is_some() {
            panic!("Cannot call both config_file() and config_toml()");
        }
        self.config_source = Some(ConfigSource::File(path.to_string()));
        self
    }

    pub fn no_auto_config_file(&mut self) -> &mut Self {
        self.no_auto_config_file = true;
        self
    }

    pub fn ignore_environment_variables(&mut self) -> &mut Self {
        self.ignore_env_vars = true;
        self
    }

    /// Initialize the tracing subscriber based on the configuration
    ///
    pub fn init(&mut self) -> Result<String, Box<dyn std::error::Error>> {
        if !self.ignore_env_vars {
            self.apply_environment_variables();
        }
        self.apply_toml_config();
        self.apply_defaults();

        let console_layer = self.get_console_layer();
        let log_file_layer = self.get_log_file_layer()?;
        let log_server_layer = self.get_log_server_layer()?;

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
        if self.log_file_rotation.is_none() {
            if let Ok(rotation_value) = std::env::var("LOG_FILE_ROTATION") {
                let (rotation, count) = Self::parse_rotation_string(&rotation_value);
                self.log_file_rotation = Some(rotation);
                self.log_file_backups = count;
            }
        }
        if self.log_server_address.is_none() {
            if let Ok(server) = std::env::var("LOG_SERVER") {
                self.log_server_address = Some(server);
            }
        }
    }

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
        if self.log_server_address.is_none() { self.log_server_address = config.server.clone(); }
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
        if self.log_file_path.is_none() { self.log_file_path = Some(String::new()); }
        if self.log_file_rotation.is_none() { self.log_file_rotation = Some(tracing_appender::rolling::Rotation::DAILY); }
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

        if let Some(ref config_source) = self.config_summary {
            if summary.is_empty() {
                summary = config_source.clone();
            } else {
                summary.push_str(&format!(", {}", config_source));
            }
        }

        summary
    }

    fn get_rotation_description(&self) -> String {
        if let Some(ref rotation) = self.log_file_rotation {
            let rotation_name = match *rotation {
                tracing_appender::rolling::Rotation::DAILY => "daily",
                tracing_appender::rolling::Rotation::HOURLY => "hourly",
                tracing_appender::rolling::Rotation::MINUTELY => "minutely",
                tracing_appender::rolling::Rotation::NEVER => "",
                _ => "weekly",
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

//! Simple tracing subscriber initialization with optional TOML configuration.
//!
//! `tracing-init` provides a builder-based API to set up [`tracing`] subscribers with
//! multiple output destinations: console, rotating log files, GELF-over-UDP server
//! (compatible with Graylog and other GELF consumers), and OpenTelemetry exporters.
//!
//! Configuration is resolved from multiple sources with a clear precedence order,
//! making it easy to ship sensible defaults while allowing per-environment and
//! per-application overrides.
//!
//! # Quick Start
//!
//! ```no_run
//! // Minimal -- auto-discovers logging.toml if present
//! let guard = tracing_init::TracingInit::builder("myapp").init().unwrap();
//! println!("Logging: {guard}");
//! ```
//!
//! # Destination-keyed API
//!
//! ```no_run
//! use tracing::Level;
//! use tracing_init::types::Format;
//!
//! let guard = tracing_init::TracingInit::builder("myapp")
//!     .destination("c")
//!     .level("*", Level::INFO)
//!     .level("console", Level::DEBUG)
//!     .format("console", Format::Pretty)
//!     .no_auto_config_file()
//!     .ignore_environment_variables()
//!     .init()
//!     .unwrap();
//! ```
//!
//! # TOML Configuration
//!
//! ```no_run
//! # #[cfg(feature = "config")]
//! # {
//! let guard = tracing_init::TracingInit::builder("myapp")
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
//! destination = "cfg"           # c=console, f=file, g=gelf, o=otel
//! level = "info"
//!
//! [logging.console]
//! format = "pretty"
//! level = "debug"
//!
//! [logging.file]
//! path = "logs"
//! rotation = "d:3"
//!
//! [logging.gelf]
//! address = "localhost:12201"
//!
//! [logging.myapp]              # Per-app overrides
//! destination = "-f+g"
//! level = "debug"
//! ```
//!
//! ## Precedence (highest to lowest)
//!
//! 1. Explicit builder calls
//! 2. Environment variables (`LOG_DESTINATION`, `LOG_LEVEL`, `RUST_LOG`)
//!    - `LOG_CONFIG` specifies a TOML config file path (used during auto-discovery)
//! 3. TOML config (app-specific over base)
//! 4. Defaults
//!
//! ## Environment Variables
//!
//! * `LOG_DESTINATION` — contains `c`, `f`, `g`, and/or `o`
//! * `LOG_LEVEL` — error, warn, info, debug, trace
//! * `RUST_LOG` — filter directive (used by `build_env_filter` fallback)
//! * `LOG_CONFIG` — path to a TOML config file (used during auto-discovery)
//!
use std::fmt::Display;

#[cfg(feature = "config")]
mod config;
#[cfg(feature = "gelf")]
mod gelf;
mod guard;
#[cfg(feature = "otel")]
mod otel;

#[cfg(test)]
mod tests;

pub use guard::TracingGuard;

pub mod types;
pub mod dest_config;

use tracing::Level;
use tracing_subscriber::Registry;
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
/// use tracing::Level;
/// use tracing_init::types::Format;
///
/// let guard = tracing_init::TracingInit::builder("myapp")
///     .destination("c")
///     .level("*", Level::INFO)
///     .format("console", Format::Pretty)
///     .no_auto_config_file()
///     .ignore_environment_variables()
///     .init()
///     .unwrap();
/// ```
#[derive(Debug, Clone)]
pub struct TracingInit {
    app_name: String,
    service_name: Option<String>,
    destination: Option<String>,
    dest_settings: dest_config::DestinationSettings,

    // Legacy enable flags (set by log_to_* methods)
    enable_console: Option<bool>,
    enable_file: Option<bool>,
    enable_gelf: Option<bool>,
    #[cfg(feature = "otel")]
    enable_otel: Option<bool>,

    // File-specific
    #[cfg(feature = "file")]
    file_path: Option<String>,
    #[cfg(feature = "file")]
    file_prefix: String,
    #[cfg(feature = "file")]
    file_rotation: Option<String>,

    // GELF-specific
    #[cfg(feature = "gelf")]
    gelf_address: Option<String>,

    // OTel-specific
    #[cfg(feature = "otel")]
    otel_endpoint: Option<String>,
    #[cfg(feature = "otel")]
    otel_transport: Option<types::Transport>,
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

type BoxedLayer = Option<Box<dyn Layer<Registry> + Send + Sync + 'static>>;

impl TracingInit {
    /// Create a new builder with the given application name.
    ///
    /// The `app_name` is used as:
    /// - The GELF `_app` additional field when logging to a server.
    /// - The default log-file prefix (e.g. `myapp.2024-01-15.log`).
    /// - The key for per-app TOML overrides (`[logging.<app_name>]`).
    ///
    /// All output destinations start as *unset*; call the `log_to_*` methods or
    /// provide a `destination` string, TOML, or environment variables to enable them.
    pub fn builder(app_name: &str) -> TracingInit {
        TracingInit {
            app_name: app_name.to_string(),
            service_name: None,
            destination: None,
            dest_settings: dest_config::DestinationSettings::new(),

            enable_console: None,
            enable_file: None,
            enable_gelf: None,
            #[cfg(feature = "otel")]
            enable_otel: None,

            #[cfg(feature = "file")]
            file_path: None,
            #[cfg(feature = "file")]
            file_prefix: app_name.to_string(),
            #[cfg(feature = "file")]
            file_rotation: None,

            #[cfg(feature = "gelf")]
            gelf_address: None,

            #[cfg(feature = "otel")]
            otel_endpoint: None,
            #[cfg(feature = "otel")]
            otel_transport: None,
            #[cfg(feature = "otel")]
            otel_resource_attrs: Vec::new(),

            #[cfg(feature = "config")]
            config_source: None,
            #[cfg(feature = "config")]
            config_summary: None,
            no_auto_config_file: false,
            ignore_env_vars: false,
        }
    }

    // ── Destination-keyed builder methods ──

    /// Set the service name for telemetry (GELF, OTel).
    pub fn service_name(&mut self, name: &str) -> &mut Self {
        self.service_name = Some(name.to_string());
        self
    }

    /// Set the destination string (e.g. `"cfg"` for console + gelf + file).
    ///
    /// Characters: `c` = console, `f` = file, `g` = gelf, `o` = otel.
    pub fn destination(&mut self, dest: &str) -> &mut Self {
        self.destination = Some(dest.to_string());
        self
    }

    /// Set log level for a destination. Use `"*"` for all destinations.
    pub fn level(&mut self, dest: &str, level: Level) -> &mut Self {
        self.dest_settings.set_level(dest, level);
        self
    }

    /// Set an EnvFilter directive for a destination. Use `"*"` for all.
    pub fn filter(&mut self, dest: &str, filter: &str) -> &mut Self {
        self.dest_settings.set_filter(dest, filter);
        self
    }

    /// Set output format for a destination.
    pub fn format(&mut self, dest: &str, format: types::Format) -> &mut Self {
        self.dest_settings.set_format(dest, format);
        self
    }

    /// Set ANSI color output for a destination.
    pub fn ansi(&mut self, dest: &str, value: bool) -> &mut Self {
        self.dest_settings.set_ansi(dest, value);
        self
    }

    /// Set timestamp display for a destination.
    pub fn timestamps(&mut self, dest: &str, value: bool) -> &mut Self {
        self.dest_settings.set_timestamps(dest, value);
        self
    }

    /// Set target display for a destination.
    pub fn target(&mut self, dest: &str, value: bool) -> &mut Self {
        self.dest_settings.set_target(dest, value);
        self
    }

    /// Set thread name display for a destination.
    pub fn thread_names(&mut self, dest: &str, value: bool) -> &mut Self {
        self.dest_settings.set_thread_names(dest, value);
        self
    }

    /// Set file/line display for a destination.
    pub fn file_line(&mut self, dest: &str, value: bool) -> &mut Self {
        self.dest_settings.set_file_line(dest, value);
        self
    }

    /// Set span events for a destination.
    pub fn span_events(&mut self, dest: &str, events: types::SpanEvents) -> &mut Self {
        self.dest_settings.set_span_events(dest, events);
        self
    }

    // ── File-specific methods ──

    /// Set the directory for log files.
    #[cfg(feature = "file")]
    pub fn file_path(&mut self, path: &str) -> &mut Self {
        self.file_path = Some(path.to_string());
        self
    }

    /// Set the log file name prefix (default: the `app_name`).
    #[cfg(feature = "file")]
    pub fn file_prefix(&mut self, prefix: &str) -> &mut Self {
        self.file_prefix = prefix.to_string();
        self
    }

    /// Set the log file rotation policy as a string (e.g. `"d:3"`).
    #[cfg(feature = "file")]
    pub fn file_rotation(&mut self, rotation: &str) -> &mut Self {
        self.file_rotation = Some(rotation.to_string());
        self
    }

    // ── OTel-specific methods ──

    /// Set the OTLP endpoint URL.
    #[cfg(feature = "otel")]
    pub fn otel_endpoint(&mut self, endpoint: &str) -> &mut Self {
        self.otel_endpoint = Some(endpoint.to_string());
        self
    }

    /// Set the OTLP transport protocol.
    #[cfg(feature = "otel")]
    pub fn otel_transport(&mut self, transport: types::Transport) -> &mut Self {
        self.otel_transport = Some(transport);
        self
    }

    /// Add an OTel resource attribute.
    #[cfg(feature = "otel")]
    pub fn otel_resource_attribute(&mut self, key: &str, value: &str) -> &mut Self {
        self.otel_resource_attrs.push((key.to_string(), value.to_string()));
        self
    }

    // ── Legacy enable methods ──

    /// Enable or disable console (stdout) logging.
    pub fn log_to_console(&mut self, v: bool) -> &mut Self {
        self.enable_console = Some(v);
        self
    }

    /// Enable or disable rotating file logging.
    #[cfg(feature = "file")]
    pub fn log_to_file(&mut self, v: bool) -> &mut Self {
        self.enable_file = Some(v);
        self
    }

    /// Enable or disable GELF-over-UDP server logging.
    #[cfg(feature = "gelf")]
    pub fn log_to_gelf_server(&mut self, v: bool) -> &mut Self {
        self.enable_gelf = Some(v);
        self
    }

    /// Enable or disable OpenTelemetry export.
    #[cfg(feature = "otel")]
    pub fn log_to_otel(&mut self, v: bool) -> &mut Self {
        self.enable_otel = Some(v);
        self
    }

    // ── Config methods ──

    /// Provide a pre-parsed [`toml::Value`] as the configuration source.
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
    pub fn no_auto_config_file(&mut self) -> &mut Self {
        self.no_auto_config_file = true;
        self
    }

    /// Ignore all `LOG_*` and `RUST_LOG` environment variables.
    pub fn ignore_environment_variables(&mut self) -> &mut Self {
        self.ignore_env_vars = true;
        self
    }

    // ── Initialization ──

    /// Resolve all configuration sources and install the global tracing subscriber.
    ///
    /// Returns a [`TracingGuard`] that holds resources and provides a summary.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The log file directory cannot be created or written to.
    /// - The GELF server address cannot be resolved.
    /// - The filter directive string is invalid.
    pub fn init(&mut self) -> Result<TracingGuard, Box<dyn std::error::Error>> {
        if !self.ignore_env_vars {
            self.apply_environment_variables();
        }
        #[cfg(feature = "config")]
        self.apply_toml_config();

        let mut layers: Vec<Box<dyn Layer<Registry> + Send + Sync + 'static>> = Vec::new();

        #[cfg(feature = "otel")]
        let mut tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider> = None;
        #[cfg(feature = "otel")]
        let mut logger_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider> = None;

        if let Some(layer) = self.get_console_layer()? {
            layers.push(layer);
        }
        #[cfg(feature = "file")]
        if let Some(layer) = self.get_file_layer()? {
            layers.push(layer);
        }
        #[cfg(feature = "gelf")]
        if let Some(layer) = self.get_gelf_layer()? {
            layers.push(layer);
        }

        // OTel layers
        #[cfg(feature = "otel")]
        {
            if self.is_dest_enabled('o') {
                let endpoint = self.otel_endpoint.as_deref().unwrap_or("http://localhost:4318");
                let transport = self.otel_transport.map(|t| t.to_string()).unwrap_or_else(|| "http".to_string());
                let service = self.service_name.as_deref().unwrap_or(&self.app_name);
                let resource = otel::build_resource(service, &self.otel_resource_attrs);

                let (tp, trace_layer) = otel::traces::create_trace_layer(endpoint, &transport, resource.clone())?;
                let (lp, log_layer) = otel::logs::create_log_layer(endpoint, &transport, resource)?;

                // Build separate filters — EnvFilter is not Clone
                let otel_filter = self.build_env_filter("otel")?;
                let otel_filter2 = self.build_env_filter("otel")?;

                layers.push(trace_layer.with_filter(otel_filter).boxed());
                layers.push(log_layer.with_filter(otel_filter2).boxed());

                // Store providers in guard for shutdown
                tracer_provider = Some(tp);
                logger_provider = Some(lp);
            }
        }

        #[cfg(not(feature = "otel"))]
        {
            if self.destination.as_ref().is_some_and(|d| d.contains('o')) {
                eprintln!("Warning: Destination 'o' requested but 'otel' feature not enabled — skipping");
            }
        }

        tracing_subscriber::registry()
            .with(layers)
            .init();

        Ok(TracingGuard {
            summary_text: self.build_summary(),
            #[cfg(feature = "file")]
            _file_guard: None, // TODO: use non_blocking writer in future
            #[cfg(feature = "otel")]
            tracer_provider,
            #[cfg(feature = "otel")]
            logger_provider,
        })
    }

    // ── Internal helpers ──

    /// Check whether a destination is enabled, consulting legacy flags first,
    /// then falling back to the destination string.
    fn is_dest_enabled(&self, ch: char) -> bool {
        // Check legacy flags first
        match ch {
            'c' => {
                if let Some(v) = self.enable_console {
                    return v;
                }
            }
            'f' => {
                #[cfg(feature = "file")]
                if let Some(v) = self.enable_file {
                    return v;
                }
            }
            'g' => {
                #[cfg(feature = "gelf")]
                if let Some(v) = self.enable_gelf {
                    return v;
                }
            }
            'o' => {
                #[cfg(feature = "otel")]
                if let Some(v) = self.enable_otel {
                    return v;
                }
            }
            _ => {}
        }
        // Fall back to destination string
        self.destination.as_ref().map_or(false, |d| d.contains(ch))
    }

    /// Build an EnvFilter for a specific destination.
    ///
    /// Resolution order:
    /// 1. Destination-specific filter directive -> use it with destination level as default
    /// 2. Destination-specific level only -> use level directly (no RUST_LOG)
    /// 3. No destination-specific settings -> base level + RUST_LOG via from_env_lossy
    fn build_env_filter(&self, dest: &str) -> Result<EnvFilter, Box<dyn std::error::Error>> {
        if let Some(filter) = self.dest_settings.resolve_filter(dest) {
            let level = self.dest_settings.resolve_level(dest).unwrap_or(Level::INFO);
            return Ok(EnvFilter::builder()
                .with_default_directive(level.into())
                .parse(filter)?);
        }
        // Destination has own level but no filter -- use level only, NO RUST_LOG
        if let Some(level) = self.dest_settings.resolve_level(dest) {
            return Ok(EnvFilter::new(level.to_string()));
        }
        // No destination-specific settings -- base level + RUST_LOG
        let base_level = self.dest_settings.resolve_level("*").unwrap_or(Level::INFO);
        Ok(EnvFilter::builder()
            .with_default_directive(base_level.into())
            .from_env_lossy())
    }

    fn get_console_layer(&self) -> Result<BoxedLayer, Box<dyn std::error::Error>> {
        if !self.is_dest_enabled('c') {
            return Ok(None);
        }
        let filter = self.build_env_filter("console")?;
        let format = self.dest_settings.resolve_format("console").unwrap_or(types::Format::Full);
        let ansi = self.dest_settings.resolve_ansi("console").unwrap_or(true);
        let target = self.dest_settings.resolve_target("console").unwrap_or(true);
        let thread_names = self.dest_settings.resolve_thread_names("console").unwrap_or(false);
        let file_line = self.dest_settings.resolve_file_line("console").unwrap_or(false);
        let span_events = self.dest_settings.resolve_span_events("console")
            .unwrap_or(types::SpanEvents::NONE)
            .to_fmt_span();

        let layer = match format {
            types::Format::Pretty => tracing_subscriber::fmt::layer()
                .pretty()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line)
                .with_line_number(file_line)
                .with_span_events(span_events)
                .with_writer(std::io::stdout)
                .boxed(),
            types::Format::Json => tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line)
                .with_line_number(file_line)
                .with_span_events(span_events)
                .with_writer(std::io::stdout)
                .boxed(),
            types::Format::Compact => tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line)
                .with_line_number(file_line)
                .with_span_events(span_events)
                .with_writer(std::io::stdout)
                .boxed(),
            types::Format::Full => tracing_subscriber::fmt::layer()
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

    #[cfg(feature = "file")]
    fn get_file_layer(&self) -> Result<BoxedLayer, Box<dyn std::error::Error>> {
        if !self.is_dest_enabled('f') {
            return Ok(None);
        }
        let filter = self.build_env_filter("file")?;
        let ansi = self.dest_settings.resolve_ansi("file").unwrap_or(false);
        let target = self.dest_settings.resolve_target("file").unwrap_or(true);
        let thread_names = self.dest_settings.resolve_thread_names("file").unwrap_or(false);
        let file_line_setting = self.dest_settings.resolve_file_line("file").unwrap_or(false);
        let span_events = self.dest_settings.resolve_span_events("file")
            .unwrap_or(types::SpanEvents::NONE)
            .to_fmt_span();

        let rotation_str = self.file_rotation.as_deref().unwrap_or("d:3");
        let (rotation, max_files) = Self::parse_rotation_string(rotation_str);
        let file_path = self.file_path.as_deref().unwrap_or("");

        let file_writer = tracing_appender::rolling::RollingFileAppender::builder()
            .filename_prefix(&self.file_prefix)
            .filename_suffix("log")
            .rotation(rotation)
            .max_log_files(max_files)
            .build(file_path)?;

        let format = self.dest_settings.resolve_format("file").unwrap_or(types::Format::Full);
        let layer = match format {
            types::Format::Json => tracing_subscriber::fmt::layer()
                .json()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line_setting)
                .with_line_number(file_line_setting)
                .with_span_events(span_events)
                .with_writer(file_writer)
                .boxed(),
            types::Format::Compact => tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line_setting)
                .with_line_number(file_line_setting)
                .with_span_events(span_events)
                .with_writer(file_writer)
                .boxed(),
            types::Format::Pretty => tracing_subscriber::fmt::layer()
                .pretty()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line_setting)
                .with_line_number(file_line_setting)
                .with_span_events(span_events)
                .with_writer(file_writer)
                .boxed(),
            types::Format::Full => tracing_subscriber::fmt::layer()
                .with_ansi(ansi)
                .with_target(target)
                .with_thread_names(thread_names)
                .with_file(file_line_setting)
                .with_line_number(file_line_setting)
                .with_span_events(span_events)
                .with_writer(file_writer)
                .boxed(),
        };
        Ok(Some(layer.with_filter(filter).boxed()))
    }

    #[cfg(feature = "gelf")]
    fn get_gelf_layer(&self) -> Result<BoxedLayer, Box<dyn std::error::Error>> {
        if !self.is_dest_enabled('g') {
            return Ok(None);
        }
        let filter = self.build_env_filter("gelf")?;
        let addr = self.gelf_address.as_deref().unwrap_or("localhost:12201");
        let service = self.service_name.as_deref().unwrap_or(&self.app_name);
        let layer = gelf::GelfLayer::new(
            addr,
            vec![("app", self.app_name.clone())],
            Some(service.to_string()),
        )?;
        Ok(Some(layer.with_filter(filter).boxed()))
    }

    /// Parse a rotation string like `"d:3"` into a rotation policy and backup count.
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

    fn apply_environment_variables(&mut self) {
        if let Ok(log_destination) = std::env::var("LOG_DESTINATION") {
            if self.destination.is_none() && self.enable_console.is_none()
                && self.enable_file.is_none() && self.enable_gelf.is_none()
            {
                self.destination = Some(log_destination);
            }
        }
        if let Ok(level_str) = std::env::var("LOG_LEVEL") {
            if self.dest_settings.resolve_level("*").is_none() {
                if let Ok(level) = level_str.parse() {
                    self.dest_settings.set_level("*", level);
                }
            }
        }
        // RUST_LOG is handled by build_env_filter via from_env_lossy
        // LOG_CONFIG is handled in apply_toml_config
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

        // Apply destination if not already set
        if self.destination.is_none() && self.enable_console.is_none() {
            if let Some(dest) = &config.destination {
                self.destination = Some(dest.clone());
            }
        }
        // Apply top-level level/filter to wildcard dest_settings if not set
        if self.dest_settings.resolve_level("*").is_none() {
            if let Some(ref level_str) = config.level {
                if let Ok(level) = level_str.parse() {
                    self.dest_settings.set_level("*", level);
                }
            }
        }
        if self.dest_settings.resolve_filter("*").is_none() {
            if let Some(ref filter) = config.filter {
                self.dest_settings.set_filter("*", filter);
            }
        }
        if self.service_name.is_none() {
            self.service_name = config.service_name.clone();
        }

        // Apply GELF address from config
        #[cfg(feature = "gelf")]
        if self.gelf_address.is_none() {
            if let Some(ref gelf) = config.gelf {
                self.gelf_address = gelf.address.clone();
            }
        }

        // Apply file settings from config
        #[cfg(feature = "file")]
        {
            if let Some(ref file) = config.file {
                if self.file_path.is_none() {
                    self.file_path = file.path.clone();
                }
                if let Some(ref prefix) = file.prefix {
                    if self.file_prefix == self.app_name {
                        self.file_prefix = prefix.clone();
                    }
                }
                if self.file_rotation.is_none() {
                    self.file_rotation = file.rotation.clone();
                }
            }
        }
    }
}

impl TracingInit {
    fn build_summary(&self) -> String {
        let mut parts = Vec::new();

        if self.is_dest_enabled('c') {
            let format = self.dest_settings.resolve_format("console").unwrap_or(types::Format::Full);
            let level = self.dest_settings.resolve_level("console")
                .or_else(|| self.dest_settings.resolve_level("*"))
                .unwrap_or(Level::INFO);
            parts.push(format!("console({}, {})", format, level));
        }
        #[cfg(feature = "file")]
        if self.is_dest_enabled('f') {
            let path = self.file_path.as_deref().unwrap_or(".");
            let path = if path.is_empty() { "." } else { path };
            let level = self.dest_settings.resolve_level("file")
                .or_else(|| self.dest_settings.resolve_level("*"))
                .unwrap_or(Level::INFO);
            let rotation_str = self.file_rotation.as_deref().unwrap_or("d:3");
            parts.push(format!("file({}/{}.log, {}, rot:{})", path, self.file_prefix, level, rotation_str));
        }
        #[cfg(feature = "gelf")]
        if self.is_dest_enabled('g') {
            let addr = self.gelf_address.as_deref().unwrap_or("localhost:12201");
            let level = self.dest_settings.resolve_level("gelf")
                .or_else(|| self.dest_settings.resolve_level("*"))
                .unwrap_or(Level::INFO);
            parts.push(format!("gelf({}, {})", addr, level));
        }
        #[cfg(feature = "otel")]
        if self.is_dest_enabled('o') {
            let endpoint = self.otel_endpoint.as_deref().unwrap_or("http://localhost:4318");
            let level = self.dest_settings.resolve_level("otel")
                .or_else(|| self.dest_settings.resolve_level("*"))
                .unwrap_or(Level::INFO);
            parts.push(format!("otel({}, {})", endpoint, level));
        }

        let mut summary = parts.join(", ");

        #[cfg(feature = "config")]
        if let Some(ref config_source) = self.config_summary {
            if summary.is_empty() {
                summary = config_source.clone();
            } else {
                summary.push_str(&format!(", {}", config_source));
            }
        }

        if summary.is_empty() {
            summary = "no destinations enabled".to_string();
        }

        summary
    }
}

impl Display for TracingInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.build_summary())
    }
}

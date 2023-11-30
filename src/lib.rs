//! Simple tracing subscriber initialization
//!
//! # Example
//! ```
//!     TracingInit::builder("App")
//!        .log_to_console(true)
//!        .log_to_file(true)
//!        .log_to_server(true)
//!        .init()
//! ```
//!
use tracing::Level;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_subscriber::{EnvFilter, Layer};

/// Holds the configuration for the tracing subscriber
pub struct TracingInit {
    app_name: String,

    enable_console: bool,
    enable_log_file: bool,
    enable_log_server: bool,

    level: Level,

    log_file_path: String,
    log_file_prefix: String,
    log_file_rotation: tracing_appender::rolling::Rotation,
    log_file_backups: usize,

    log_server_name: String,
    log_server_port: u16,

    filter: Option<String>,
}

type BoxLayer<S> = Option<Box<dyn Layer<S> + Send + Sync + 'static>>;

impl TracingInit {
    /// Create a new TraceInit with default values
    ///
    /// # Arguments
    ///    app_name - The application name. When sending logs to server, this name will be used as for the app field
    ///
    pub fn builder(app_name: &str) -> TracingInit {
        TracingInit {
            app_name: app_name.to_string(),
            enable_console: false,
            enable_log_file: false,
            enable_log_server: false,

            level: Level::INFO,

            log_file_path: "".to_string(),
            log_file_prefix: app_name.to_string(),
            log_file_rotation: tracing_appender::rolling::Rotation::DAILY,
            log_file_backups: 3,

            log_server_name: "logging-server".to_string(),
            log_server_port: 12201,

            filter: None,
        }
    }

    /// determine if the console should be used for logging (default false)
    ///
    pub fn log_to_console(&mut self, v: bool) -> &mut Self {
        self.enable_console = v;
        self
    }

    /// determine if the log file should be used for logging (default false)
    ///
    pub fn log_to_file(&mut self, v: bool) -> &mut Self {
        self.enable_log_file = v;
        self
    }

    /// determine if the logs should be send using GELF protocol (default false)
    ///
    /// # Notes
    /// Sending log to server works only if working under async runtime (e.g. tokio)
    ///
    pub fn log_to_server(&mut self, v: bool) -> &mut Self {
        self.enable_log_server = v;
        self
    }

    /// Set the default log level (default: INFO)
    ///
    pub fn level(&mut self, level: Level) -> &mut Self {
        self.level = level;
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
        self.log_file_rotation = rotation;
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

    /// Set the address of the logging server (default: logging-server)
    ///
    /// # Notes
    /// It is advisable to add CNAME record with this name to point to the actual logging server
    ///
    pub fn log_server_address(&mut self, name: &str) -> &mut Self {
        self.log_server_name = name.to_string();
        self
    }

    /// Initialize the tracing subscriber based on the configuration
    ///
    pub fn init(&self) -> Result<(), Box<dyn std::error::Error>> {
        let console_layer = self.get_console_layer();
        let log_file_layer = self.get_log_file_layer()?;
        let log_server_layer = self.get_log_server_layer()?;

        let env_filter = if let Some(ref filter) = self.filter {
            EnvFilter::try_new(filter)?
        } else {
            EnvFilter::builder()
                .with_default_directive(self.level.into())
                .from_env_lossy()
        };

        tracing_subscriber::registry()
            .with(console_layer)
            .with(log_file_layer)
            .with(log_server_layer)
            .with(env_filter)
            .init();

        Ok(())
    }

    fn get_console_layer<S>(&self) -> Option<Box<dyn Layer<S> + Send + Sync + 'static>>
    where
        S: tracing::Subscriber,
        for<'a> S: LookupSpan<'a>,
    {
        if self.enable_console {
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

    fn get_log_file_layer<S>(&self) -> Result<BoxLayer<S>, Box<dyn std::error::Error>>
    where
        S: tracing::Subscriber,
        for<'a> S: LookupSpan<'a>,
    {
        if self.enable_log_file {
            let file_writer = tracing_appender::rolling::RollingFileAppender::builder()
                .filename_prefix(&self.log_file_prefix)
                .filename_suffix("log")
                .rotation(self.log_file_rotation.clone())
                .max_log_files(self.log_file_backups)
                .build(&self.log_file_path)?;

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

    fn get_log_server_layer<S>(&self) -> Result<BoxLayer<S>, Box<dyn std::error::Error>>
    where
        S: tracing::Subscriber,
        for<'a> S: LookupSpan<'a>,
    {
        if self.enable_log_server {
            let (gelf_layer, mut connection_task) = tracing_gelf::Logger::builder()
                .additional_field("app", self.app_name.clone())
                .connect_udp((self.log_server_name.clone(), self.log_server_port))?;

            tokio::spawn(async move {
                let connection_errors = connection_task.connect().await;

                if !connection_errors.0.is_empty() {
                    println!("Failed to connect to log server: {:?}", connection_errors);
                }
            });

            Ok(Some(gelf_layer.boxed()))
        } else {
            Ok(None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing::event;

    #[tokio::test]
    async fn test_logging() {
        TracingInit::builder("App")
            .log_to_console(true)
            .log_to_file(true)
            .log_to_server(true)
            .init()
            .unwrap();

        event!(Level::INFO, "test");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

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
//! It is possible to specify the values of the tracing subscriber using environment variables:
//! * LOG_DESTINATION - the value should contain one or more of the following characters: 'c' - console, 'f' - file, 's' - server
//! * LOG_FILE_PATH - the path to the log file
//! * LOG_FILE_ROTATION - the rotation of the log file. The value should be in the format:
//!   <rotation>[:<count>] where rotation is one of the following: d - daily, h - hourly, m - minutely, n - never and count is the number of backups to keep
//! * LOG_SERVER - the address of the logging server in the format <host>:<port>
//! * LOG_LEVEL - the log level for the tracing subscriber (error, warn, info, debug, trace)
//! * RUST_LOG - logging filter ()
//!
//! So if you use the code:
//! ```
//!    TracingInit::builder("App").init().unwrap();
//! ```
//!
//! And run the application using the command:
//! ```
//!   LOG_DESTINATION=cf app
//! ```
//!
//! The application will log to console and file (named App<date>.log) using INFO level
//! 
//! This crate also implements the Display trait for the TracingInit structure so it is possible to print the current configuration using:
//! ```
//!   println!("{}", TracingInit::builder("App").init().unwrap());
//! ```
//!
use std::fmt::Display;

use tracing::Level;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use tracing_subscriber::{EnvFilter, Layer};

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

    /// Set unspecified values of trace initialization structure based on values of the environment variables
    ///
    pub fn set_from_environment_variables(&mut self) -> &mut Self {
        let log_destination = &std::env::var("LOG_DESTINATION");

        self.enable_console = self.enable_console.or_else(|| {
            Some(
                log_destination
                    .as_ref()
                    .map(|v| v.contains('c'))
                    .unwrap_or(false),
            )
        });

        self.enable_log_file = self.enable_log_file.or_else(|| {
            Some(
                log_destination
                    .as_ref()
                    .map(|v| v.contains('f'))
                    .unwrap_or(false),
            )
        });

        self.enable_log_server = self.enable_log_server.or_else(|| {
            Some(
                log_destination
                    .as_ref()
                    .map(|v| v.contains('s'))
                    .unwrap_or(false),
            )
        });

        self.log_file_path = self
            .log_file_path
            .clone()
            .or_else(|| Some(std::env::var("LOG_FILE_PATH").unwrap_or_default()));

        self.level = self.level.or_else(|| {
            Some(
                std::env::var("LOG_LEVEL")
                    .unwrap_or(String::from("INFO"))
                    .parse()
                    .unwrap_or(Level::INFO),
            )
        });

        (self.log_file_rotation, self.log_file_backups) =
            if let Some(rotation) = self.log_file_rotation.clone() {
                (Some(rotation), self.log_file_backups)
            } else if let Ok(rotation_value) = std::env::var("LOG_FILE_ROTATION") {
                let mut rotation_value = rotation_value.split(':');
                let rotation = rotation_value.next().unwrap_or("d");
                let count = rotation_value
                    .next()
                    .map(|v| v.parse().unwrap_or(3))
                    .unwrap_or(3);

                match rotation {
                    "d" => (Some(tracing_appender::rolling::Rotation::DAILY), count),
                    "h" => (Some(tracing_appender::rolling::Rotation::HOURLY), count),
                    "m" => (Some(tracing_appender::rolling::Rotation::MINUTELY), count),
                    "n" => (Some(tracing_appender::rolling::Rotation::NEVER), count),
                    _ => (Some(tracing_appender::rolling::Rotation::DAILY), count),
                }
            } else {
                (
                    Some(tracing_appender::rolling::Rotation::DAILY),
                    self.log_file_backups,
                )
            };

        self.log_server_address = self.log_server_address.clone().or_else(|| {
            Some(std::env::var("LOG_SERVER").unwrap_or(String::from("logging-server:12201")))
        });

        self
    }

    /// Initialize the tracing subscriber based on the configuration
    ///
    pub fn init(&mut self) -> Result<&Self, Box<dyn std::error::Error>> {
        self.set_from_environment_variables();

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

        Ok(self)
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
            let (gelf_layer, mut connection_task) = tracing_gelf::Logger::builder()
                .additional_field("app", self.app_name.clone())
                .connect_udp(self.log_server_address.as_ref().unwrap().clone())?;

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

impl Display for TracingInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let console_part = if let Some(enable_console) = self.enable_console {
            if enable_console {
                "log to console"
            } else {
                ""
            }
        } else {
            "enable_console: not initialized"
        };

        let file_part = if let Some(enable_log_file) = self.enable_log_file {
            if enable_log_file {
                let path = self.log_file_path.clone().unwrap_or(String::from("lof_file_path not initialized"));

                format!(
                    "log to file {path}/{app}.log, rotation {rotation}",
                    path = if path.is_empty() { "." } else { &path },
                    app = self.log_file_prefix,
                    rotation = self.get_rotation_description()
                )
            } else {
                String::new()
            }
        } else {
            String::from("enable_log_file not initialized")
        };

        let server_part = if let Some(enable_log_server) = self.enable_log_server {
            if enable_log_server {
                format!(
                    "log to server {}",
                    self.log_server_address.as_ref().unwrap()
                )
            } else {
                String::new()
            }
        } else {
            String::from("enable_log_server not initialized")
        };

        let mut logging = Vec::<String>::new();

        if !console_part.is_empty() {
            logging.push(console_part.to_string());
        }

        if !file_part.is_empty() {
            logging.push(file_part);
        }

        if !server_part.is_empty() {
            logging.push(server_part);
        }

        let logging = logging.join(", ");

        if !logging.is_empty() {
            write!(f, "{}", logging)?;
            write!(
                f,
                "{level}",
                level = if let Some(level) = self.level {
                    format!(", default level: {}", level)
                } else {
                    String::from(", level not initialized")
                }
            )?;
            write!(
                f,
                "{filter}",
                filter = if let Some(filter) = self.filter.as_ref() {
                    format!(", ({filter})")
                } else {
                    String::new()
                }
            )?;
        }
        Ok(())
    }
}

impl TracingInit {
    fn get_rotation_description(&self) -> String {
        if let Some(ref rotation) = self.log_file_rotation {
            let rotation_name = match *rotation {
                tracing_appender::rolling::Rotation::DAILY => "daily",
                tracing_appender::rolling::Rotation::HOURLY => "hourly",
                tracing_appender::rolling::Rotation::MINUTELY => "minutely",
                tracing_appender::rolling::Rotation::NEVER => "",
            };

            if *rotation != tracing_appender::rolling::Rotation::NEVER {
                format!("rotation: {}:{}", rotation_name, self.log_file_backups)
            } else {
                String::new()
            }
        } else {
            String::from("log_file_rotation not initialized")
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tracing::event;

    #[tokio::test]
    async fn test_full_logging() {
        let t = TracingInit::builder("App")
            .log_to_console(true)
            .log_to_file(true)
            .log_to_server(true)
            .init()
            .unwrap()
            .to_string();

        println!("{}", t);

        event!(Level::INFO, "test");
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    #[tokio::test]
    async fn test_default_logging() {
        let t = TracingInit::builder("App").init().unwrap().to_string();

        println!("{}", t);

        event!(Level::INFO, "test");
    }

}

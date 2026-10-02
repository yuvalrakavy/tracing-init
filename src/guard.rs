//! Guard returned by [`TracingInit::init()`] that holds resources and flushes on drop.

use std::fmt;

use crate::sink;

/// Holds logging resources that must live for the application lifetime.
///
/// When dropped, performs an orderly shutdown, every step of it bounded:
/// 1. Stop the log-loss monitor, and report the total of every destination that lost lines
///    (at most 0.5 s)
/// 2. Flush and shut down the OTel TracerProvider and LoggerProvider, if active (at most 1 s
///    each)
/// 3. Flush the console, the log file and tracing-init's stderr notes, each on a thread of its
///    own (at most 1.5 s for all)
///
/// A destination that is stuck keeps what it still holds; nothing waits on it past its bound.
///
/// If the process exits without dropping the guard — `std::process::exit` runs no destructors —
/// an exit hook does steps 1 and 3, so a line logged just before the exit still reaches the
/// console and the file. Whichever comes first does them; nothing runs twice.
///
/// Constructed directly via struct literal in `init()`. Use `summary_only()` for testing.
pub struct TracingGuard {
    pub(crate) summary_text: String,
    #[cfg(feature = "otel")]
    pub(crate) tracer_provider: Option<opentelemetry_sdk::trace::SdkTracerProvider>,
    #[cfg(feature = "otel")]
    pub(crate) logger_provider: Option<opentelemetry_sdk::logs::SdkLoggerProvider>,
    #[cfg(feature = "otel")]
    pub(crate) beacon_handle: Option<tokio::task::JoinHandle<()>>,
}

impl TracingGuard {
    /// Create a guard with only a summary string (for testing).
    #[cfg(test)]
    pub(crate) fn summary_only(summary: String) -> Self {
        TracingGuard {
            summary_text: summary,
            #[cfg(feature = "otel")]
            tracer_provider: None,
            #[cfg(feature = "otel")]
            logger_provider: None,
            #[cfg(feature = "otel")]
            beacon_handle: None,
        }
    }

    /// Returns a human-readable summary of the active logging setup.
    pub fn summary(&self) -> &str {
        &self.summary_text
    }
}

impl Drop for TracingGuard {
    fn drop(&mut self) {
        // Taken from the exit hook's reach: from here on, the guard ends the destinations.
        let mut ending = sink::take_ending();
        // First, while every destination still takes lines: the report reaches the OTel log
        // bridge before its shutdown and the stream workers before their flush.
        if let Some(ending) = ending.as_mut() {
            ending.report();
        }
        #[cfg(feature = "otel")]
        {
            // Abort beacon listener before shutting down providers
            if let Some(handle) = self.beacon_handle.take() {
                handle.abort();
            }
            // opentelemetry_sdk 0.31's default `provider.shutdown()` blocks
            // up to 30s waiting for the BatchSpanProcessor to flush its
            // pending queue. When the OTel collector is unreachable
            // (which is the common case for short-lived CLI tools and
            // test runners), that's a 30-second hang on every program
            // exit. Use `shutdown_with_timeout(1s)` instead — the
            // circuit breaker has already filtered out broken
            // destinations during the run, so anything still queued is
            // fine to drop.
            const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
            if let Some(ref provider) = self.tracer_provider {
                let _ = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
            }
            if let Some(ref provider) = self.logger_provider {
                let _ = provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
            }
        }
        // Last, so the notes the OTel shutdown wrote are flushed too.
        sink::flush_ending(ending);
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

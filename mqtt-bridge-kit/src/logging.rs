//! The bridge's logging (Store `docs/guides/logging-policy.md`; the fleet's no-hang 3b rules):
//! tracing-init with lossy destinations, `on_destination_error(Skip)` pinned in code, a file under
//! `logs/` and GELF, whatever `logging.toml` adds. Started on a blocking thread and raced against
//! the stop by [`crate::process`]; the guard is kept for the life of the bridge.
//!
//! The one message the bridge writes to stderr itself is the logging's own failure — there is no
//! log to say it in — and it is written from the start's thread, so a stderr nobody drains holds
//! only that thread, never the stop.

use std::path::Path;

use tracing_init::types::OnDestinationError;
use tracing_init::TracingGuard;

/// The test seam's environment variable (debug builds only): see [`hold_for_test`].
pub const TEST_LOGGING_GATE: &str = "MQTT_BRIDGE_TEST_LOGGING_GATE";

/// tracing-init's start, synchronous, on the raced start's thread.
pub(crate) fn start(app: &str, config: Option<&Path>) -> Option<TracingGuard> {
    hold_for_test();
    let mut builder = tracing_init::TracingInit::builder(app);
    builder
        .log_to_file(true)
        .log_to_gelf_server(true)
        .file_prefix(app)
        .file_path("logs")
        // Telemetry never stops the bridge: a destination that fails is skipped whatever
        // logging.toml says, and if the logging cannot start at all the bridge runs without it.
        .on_destination_error(OnDestinationError::Skip);
    if let Some(config) = config {
        builder.config_file(&config.to_string_lossy());
    }
    match builder.init() {
        Ok(guard) => Some(guard),
        Err(e) => {
            eprintln!("Logging did not start ({e}); running without it");
            None
        }
    }
}

/// The test seam for a logging start that does not end (debug builds only; a release build has
/// none): when `MQTT_BRIDGE_TEST_LOGGING_GATE` names a file, the start reads it first. A test points
/// it at a FIFO whose write end it holds open and never writes, so the read — and the start — never
/// returns until the test lets go, as on a stalled file system. Unset, it does nothing.
#[cfg(debug_assertions)]
fn hold_for_test() {
    if let Some(gate) = std::env::var_os(TEST_LOGGING_GATE) {
        let _ = std::fs::read(gate);
    }
}

#[cfg(not(debug_assertions))]
fn hold_for_test() {}

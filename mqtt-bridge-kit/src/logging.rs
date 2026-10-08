//! The bridge's logging (Store `docs/guides/logging-policy.md`; the fleet's no-hang 3b rules):
//! tracing-init with lossy destinations, `on_destination_error(Skip)` pinned in code, a file under
//! `logs/` and GELF, whatever `logging.toml` adds. Started on a blocking thread and raced against
//! the stop by [`crate::process`]; the guard is kept for the life of the bridge.
//!
//! The one message the bridge writes to stderr itself is the logging's own failure — there is no
//! log to say it in — and it is written by a thread of its own, never joined: a stderr nobody
//! drains holds only that thread, neither the start, which `serve` waits for, nor the stop
//! (review MINOR 1: written on the start's thread, it held the start for good).

use std::path::Path;

use tracing_init::types::OnDestinationError;
use tracing_init::TracingGuard;

/// The test seam's environment variable (with the `test-seams` feature only): see [`hold_for_test`].
pub const TEST_LOGGING_GATE: &str = "MQTT_BRIDGE_TEST_LOGGING_GATE";

/// tracing-init's start, synchronous, on the raced start's thread.
pub(crate) fn start(app: &str, config: Option<&Path>) -> Option<TracingGuard> {
    if hold_for_test() {
        say_on_stderr("Logging did not start (the test seam failed it); running without it".to_owned());
        return None;
    }
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
            say_on_stderr(format!("Logging did not start ({e}); running without it"));
            None
        }
    }
}

/// Write `line` to stderr from a thread of its own, never joined (see the module documentation).
fn say_on_stderr(line: String) {
    let _ = std::thread::Builder::new().name("logging-failed".into()).spawn(move || {
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), "{line}");
    });
}

/// The test seam for a logging start that does not end, or fails (with the `test-seams` feature
/// only; a build without it has none): when `MQTT_BRIDGE_TEST_LOGGING_GATE` names a file, the start reads it first. A
/// test points it at a FIFO whose write end it holds open and never writes, so the read — and the
/// start — never returns until the test lets go, as on a stalled file system; or at a file that
/// says `fail`, and the start fails. Unset, it does nothing. Returns whether to fail.
#[cfg(feature = "test-seams")]
pub(crate) fn hold_for_test() -> bool {
    match std::env::var_os(TEST_LOGGING_GATE) {
        Some(gate) => std::fs::read(gate).is_ok_and(|text| text.starts_with(b"fail")),
        None => false,
    }
}

#[cfg(not(feature = "test-seams"))]
pub(crate) fn hold_for_test() -> bool {
    false
}

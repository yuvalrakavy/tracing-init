//! Minimal example: console output only, configured via the builder.
//!
//! Run with:
//! ```sh
//! cargo run --example console_only
//! ```
//!
//! Try setting `LOG_LEVEL=debug` or `RUST_LOG=console_only=trace` to see how
//! per-run environment overrides interact with the builder configuration.

use tracing::{debug, error, info, instrument, warn};
use tracing_init::{types::Format, TracingInit};

#[instrument]
fn do_work(item: u32) {
    info!(item, "processing");
    if item % 2 == 0 {
        debug!("even branch");
    } else {
        warn!("odd branch");
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _guard = TracingInit::builder("console_only")
        .destination("c")
        .format("console", Format::Pretty)
        .span_events("console", tracing_init::types::SpanEvents::ALL)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()?;

    info!("starting");
    for i in 0..3 {
        do_work(i);
    }
    error!("simulated failure");
    info!("done");
    Ok(())
}

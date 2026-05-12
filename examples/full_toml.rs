//! TOML-driven example. Reads `examples/full_toml.logging.toml` and lets the
//! file decide which destinations are active.
//!
//! Run with:
//! ```sh
//! cargo run --example full_toml
//! ```
//!
//! Edit `examples/full_toml.logging.toml` to flip destinations, change
//! formats, or point GELF at a real collector — no recompilation required.

use tracing::{info, instrument, warn};
use tracing_init::TracingInit;

#[instrument(fields(user_id = %user_id))]
fn handle_request(user_id: u32) {
    info!("received");
    if user_id == 0 {
        warn!("anonymous user");
    }
    info!(latency_ms = 7, "completed");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let guard = TracingInit::builder("full_toml")
        .config_file("examples/full_toml.logging.toml")
        .init()?;
    println!("active destinations: {guard}");

    info!(version = env!("CARGO_PKG_VERSION"), "service starting");
    for user_id in [0, 42, 7] {
        handle_request(user_id);
    }
    info!("service shutting down");
    Ok(())
}

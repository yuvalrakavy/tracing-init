//! Demonstrates the OpenTelemetry circuit-breaker + beacon behavior.
//!
//! Run with:
//! ```sh
//! cargo run --example otel_resilient --features otel
//! ```
//!
//! Behavior:
//!
//! - With no OTel collector running, you should see exactly ONE line on
//!   stderr ("OTel collector not online…") and the program should exit in
//!   well under a second — no `Connection refused` flood, no 30-second
//!   shutdown hang.
//! - Start a collector on `localhost:4318` and run again; you'll see "OTel
//!   collector online, sending traces" once and the spans below will be
//!   exported.
//! - With the beacon group joined, you can flip the circuit in real time
//!   by sending `OTEL:ONLINE` / `OTEL:OFFLINE` (plus newline) packets to
//!   `239.255.77.1:4399`.

#[cfg(feature = "otel")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use tracing::{info, instrument};
    use tracing_init::TracingInit;

    // Build the runtime manually because the crate's `tokio` dependency
    // doesn't pull in the `macros` feature; using `tokio::runtime::Runtime`
    // directly keeps the example self-contained.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let guard = TracingInit::builder("otel_resilient")
        .service_name("otel-resilient-example")
        .destination("co")
        .otel_endpoint("http://localhost:4318")
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()?;
    println!("active destinations: {guard}");

    #[instrument]
    async fn inner_work(step: u32) {
        info!(step, "working");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    runtime.block_on(async {
        for i in 0..5 {
            inner_work(i).await;
        }
        info!("done");
    });
    Ok(())
}

#[cfg(not(feature = "otel"))]
fn main() {
    eprintln!("This example requires the `otel` feature.");
    eprintln!("Re-run with: cargo run --example otel_resilient --features otel");
    std::process::exit(2);
}

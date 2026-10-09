//! A multi-device host process on the Bridge Runtime, building fake devices: the host's process
//! scenarios run it against `FakeBroker`. Kind `fakekind`. `--block <instance>` makes that
//! instance's factory block for 30 s (a factory that never returns, past `--factory-timeout`).

use std::process::ExitCode;
use std::time::Duration;

use mqtt_bridge_conformance::acknowledged;
use mqtt_bridge_conformance::host::{Build, Fleet, KIND};

fn main() -> ExitCode {
    let host = mqtt_bridge_kit::Host::new(KIND).option("block", "an instance whose factory blocks for 30 s").from_cli();
    let fleet = Fleet::new(acknowledged());
    if let Some(instance) = host.value("block") {
        fleet.script(instance, &[Build::Block(Duration::from_secs(30))]);
    }
    host.device(fleet.factory()).run()
}

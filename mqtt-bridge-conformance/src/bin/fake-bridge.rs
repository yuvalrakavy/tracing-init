//! A bridge process on the Bridge Runtime, driving the fake device: the process scenarios run it
//! against `FakeBroker` and signal it. `--device` sets the device: `up` (the default), `down`
//! (every connect refused) or `stall-connect` (a connect that never returns until its deadline).

use std::process::ExitCode;

use mqtt_bridge_conformance::{acknowledged, fake_device, Answer, Op};

fn main() -> ExitCode {
    let bridge = mqtt_bridge_kit::Bridge::new("Fake").option("device", "up | down | stall-connect").from_cli();
    let (driver, device) = fake_device(acknowledged());
    device.set("t1", "power", serde_json::json!(true));
    match bridge.value("device").unwrap_or("up") {
        "down" => device.set_up(false),
        "stall-connect" => device.answer(Op::Connect, None, Answer::Hang),
        _ => {}
    }
    bridge.run(driver)
}

//! A bridge process on the Bridge Runtime, driving the fake device: the process scenarios run it
//! against `FakeBroker` and signal it. `--device` sets the device: `up` (the default), `down`
//! (every connect refused), `stall-connect` (a connect that never returns until its deadline),
//! `panic-apply` (the driver panics on its first `apply`), `panic-helper` (a helper that panics
//! 100 ms into every link) or `hang-info` (the driver's `info()` writes `info-entered` in the
//! working directory, then blocks for 30 s).

use std::process::ExitCode;

use mqtt_bridge_conformance::{acknowledged, fake_device, Answer, Op};

fn main() -> ExitCode {
    let bridge = mqtt_bridge_kit::Bridge::new("Fake")
        .option("device", "up | down | stall-connect | panic-apply | panic-helper | hang-info")
        .from_cli();
    let (driver, device) = fake_device(acknowledged());
    device.set("t1", "power", serde_json::json!(true));
    match bridge.value("device").unwrap_or("up") {
        "down" => device.set_up(false),
        "stall-connect" => device.answer(Op::Connect, None, Answer::Hang),
        "panic-apply" => device.answer(Op::Apply, None, Answer::Panic),
        "panic-helper" => device.helper_every_link(mqtt_bridge_conformance::Helper::PanicAfter(std::time::Duration::from_millis(100))),
        "hang-info" => device.hold_info("info-entered".into(), std::time::Duration::from_secs(30)),
        _ => {}
    }
    bridge.run(driver)
}

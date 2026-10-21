//! A bridge process on the Bridge Runtime, driving the fake device: the process scenarios run it
//! against `FakeBroker` and signal it. `--device` sets the device: `up` (the default), `down`
//! (every connect refused), `stall-connect` (a connect that never returns until its deadline),
//! `panic-apply` (the driver panics on its first `apply`), `hang-apply` (every `apply` hangs until
//! its deadline), `panic-helper` (a helper that panics 100 ms into every link) or `hang-info` (the
//! driver's `info()` writes `info-entered` in the working directory, then blocks for 30 s).

use std::process::ExitCode;

use mqtt_bridge_conformance::{acknowledged, fake_device, Answer, FakeDevice, Op};
use mqtt_bridge_kit::{ProcessService, ServiceContext};

struct Managed {
    driver: FakeDevice,
    mode: String,
}
struct Resource {
    blocked_drop: bool,
}
impl Drop for Resource {
    fn drop(&mut self) {
        if self.blocked_drop {
            std::fs::write("drop-entered", "yes").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(30));
        }
        let _ = std::fs::remove_file("service-live");
    }
}
impl ProcessService for Managed {
    type Driver = FakeDevice;
    async fn run(self, mut ctx: ServiceContext<FakeDevice>) -> Result<(), String> {
        let _resource = Resource { blocked_drop: self.mode == "blocked-drop" };
        let _listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.map_err(|e| e.to_string())?;
        std::fs::write("service-live", _listener.local_addr().unwrap().to_string()).unwrap();
        if self.mode != "no-ready" {
            ctx.ready(self.driver)?;
        }
        match self.mode.as_str() {
            "blocked" => std::thread::sleep(std::time::Duration::from_secs(30)),
            "fail" | "panic" => {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                assert_ne!(self.mode, "panic", "managed process panic");
                return Err("managed process failure".into());
            }
            _ => {}
        }
        ctx.cancelled().await;
        Ok(())
    }
}

fn main() -> ExitCode {
    let bridge = mqtt_bridge_kit::Bridge::new("Fake")
        .option("device", "up | down | stall-connect | panic-apply | hang-apply | panic-helper | hang-info")
        .option("service", "up | no-ready | blocked | blocked-drop | fail | panic (managed process tests)")
        .from_cli();
    let (driver, device) = fake_device(acknowledged());
    device.set("t1", "power", serde_json::json!(true));
    match bridge.value("device").unwrap_or("up") {
        "down" => device.set_up(false),
        "stall-connect" => device.answer(Op::Connect, None, Answer::Hang),
        "panic-apply" => device.answer(Op::Apply, None, Answer::Panic),
        "hang-apply" => device.answer(Op::Apply, None, Answer::Hang),
        "panic-helper" => device.helper_every_link(mqtt_bridge_conformance::Helper::PanicAfter(std::time::Duration::from_millis(100))),
        "hang-info" => device.hold_info("info-entered".into(), std::time::Duration::from_secs(30)),
        _ => {}
    }
    if let Some(mode) = bridge.value("service").map(str::to_owned) {
        bridge.run_managed(Managed { driver, mode })
    } else {
        bridge.run(driver)
    }
}

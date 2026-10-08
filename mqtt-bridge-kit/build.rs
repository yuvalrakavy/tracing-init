//! The `test-seams` feature's guard (re-gate 3 S6): the seams compile only where the environment
//! says this is the conformance suite's build — `MQTT_BRIDGE_TEST_SEAMS=1`, which this repository's
//! `.cargo/config.toml` sets — so a workspace that unifies the feature into a deployed bridge fails
//! to compile instead of shipping them.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(mqtt_bridge_test_seams)");
    println!("cargo::rerun-if-env-changed=MQTT_BRIDGE_TEST_SEAMS");
    if std::env::var("MQTT_BRIDGE_TEST_SEAMS").is_ok_and(|v| v == "1") {
        println!("cargo::rustc-cfg=mqtt_bridge_test_seams");
    }
}

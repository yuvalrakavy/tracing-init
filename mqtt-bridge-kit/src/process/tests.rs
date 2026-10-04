//! The bounded shutdown's backstop.

use super::*;
use crate::test_support::Capture;

/// A shutdown that does not end within its bound is cut short, with one WARN of the fleet's kind
/// (`shutdown_timeout`, not an ERROR: nothing in the code is wrong when a stop is slow).
#[tokio::test]
async fn a_shutdown_past_its_bound_is_cut_short_with_one_warn() {
    let log = Capture::start();
    let never = std::future::pending::<()>();
    let ended = tokio::time::timeout(Duration::from_secs(5), stop_within(Duration::from_millis(50), never)).await;
    assert_eq!(ended, Ok(false), "a shutdown that never ends was not cut short at its bound");
    let warnings = log.at_least(tracing::Level::WARN);
    assert!(
        warnings.len() == 1 && warnings[0].kind == "shutdown_timeout" && warnings[0].field("grace_ms") == Some("50"),
        "the cut-short shutdown was not one WARN `shutdown_timeout` with its bound; the log: {:#?}",
        log.records()
    );
}

/// A driver's required option, missing, is a usage error (exit 2) without a driver; a value the
/// driver cannot use is one too (`usage_error`), said with the usage text (the mqtt_ac v2 build:
/// its `main` re-ran the parse and handed `run` a dummy driver to print it).
#[test]
fn a_drivers_required_option_and_its_own_usage_errors_exit_2_without_a_driver() {
    let args = |line: &str| line.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    let bridge = Bridge::new("A").option("coolmaster", "its address").require("coolmaster").args(args("--instance i --broker x"));
    assert_eq!(bridge.usage_exit(), Some(ExitCode::from(2)), "a missing required option was not a usage error");
    let bridge =
        Bridge::new("A").option("coolmaster", "its address").require("coolmaster").args(args("--instance i --broker x --coolmaster h"));
    assert_eq!(bridge.usage_exit(), None, "a given required option was refused");
    assert_eq!(bridge.value("coolmaster"), Some("h"));
    let refused = bridge.usage_error("`h` is not a CoolMaster address");
    assert_eq!(refused.usage_exit(), Some(ExitCode::from(2)), "the driver's own usage error did not exit 2");
    assert_eq!(Bridge::new("A").args(args("--help")).usage_exit(), Some(ExitCode::SUCCESS));
}

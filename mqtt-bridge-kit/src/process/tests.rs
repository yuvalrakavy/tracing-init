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

/// The test seams are off in a build without the `test-seams` feature — a debug build deployed
/// included, which used to honour them (Claude's re-gate minor 4: gated on `debug_assertions`).
#[cfg(unix)]
#[cfg(not(feature = "test-seams"))]
#[tokio::test]
async fn the_test_seams_are_off_without_their_feature() {
    std::env::set_var(TEST_NO_SIGNALS, "1");
    let registered = StopSignals::register();
    std::env::remove_var(TEST_NO_SIGNALS);
    assert!(registered.is_ok(), "the no-signals seam is on in a build without the test-seams feature");
}

/// The logging start's seam is off without the feature too (the re-gate 3 S5: one row covered the
/// signals seam alone).
#[cfg(not(feature = "test-seams"))]
#[test]
fn the_logging_seam_is_off_without_its_feature() {
    let gate = std::env::temp_dir().join(format!("logging-gate-{}", std::process::id()));
    std::fs::write(&gate, b"fail").expect("the gate file");
    std::env::set_var(crate::logging::TEST_LOGGING_GATE, &gate);
    let fails = crate::logging::hold_for_test();
    std::env::remove_var(crate::logging::TEST_LOGGING_GATE);
    let _ = std::fs::remove_file(&gate);
    assert!(!fails, "the logging seam is on in a build without the test-seams feature");
}

/// A pause of the whole process starts a grace; the watchdog judges again only once it is over,
/// and ordinary ticks judge at once (re-gate 3 S6).
#[test]
fn a_pause_starts_a_grace_before_the_watchdog_judges() {
    let t0 = std::time::Instant::now();
    let mut pause = PauseGrace::new(t0);
    assert!(pause.judging(t0 + WATCH_TICK), "an ordinary tick did not judge");
    let woke = t0 + WATCH_TICK + Duration::from_secs(10);
    assert!(!pause.judging(woke), "a pause was taken for a dead worker: the tick after it judged");
    assert!(!pause.judging(woke + WATCH_TICK), "a pause was taken for a dead worker: a tick in its grace judged");
    let mut tick = woke + WATCH_TICK;
    while tick < woke + PAUSE_GRACE {
        tick += WATCH_TICK;
        pause.judging(tick);
    }
    assert!(pause.judging(tick + WATCH_TICK), "the grace never ended");
}
#[tokio::test]
async fn shutdown_rejects_ready_work_after_its_absolute_deadline() {
    let polled = std::sync::atomic::AtomicBool::new(false);
    let deadline = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
    assert!(
        !super::stop_before(deadline, std::time::Duration::from_secs(5), async {
            polled.store(true, std::sync::atomic::Ordering::SeqCst);
        })
        .await
    );
    assert!(!polled.load(std::sync::atomic::Ordering::SeqCst));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(10);
    assert!(
        !super::stop_before(deadline, std::time::Duration::from_secs(5), async {
            std::thread::sleep(std::time::Duration::from_millis(30));
        })
        .await,
        "ready completion after a delayed executor poll extended shutdown"
    );
}

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

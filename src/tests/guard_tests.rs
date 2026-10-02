use crate::guard::TracingGuard;

#[test]
fn test_summary_console_only() {
    let guard = TracingGuard::summary_only("console (full, INFO)".to_string());
    assert_eq!(guard.summary(), "console (full, INFO)");
}

#[test]
fn test_display_delegates_to_summary() {
    let guard = TracingGuard::summary_only("console (full, INFO)".to_string());
    assert_eq!(format!("{guard}"), "console (full, INFO)");
}

/// A program keeps its guard where it likes — a static, an `Arc` shared with a shutdown
/// thread — so the guard stays `Send + Sync`.
#[test]
fn the_guard_can_be_kept_anywhere() {
    fn send_and_sync<T: Send + Sync>() {}
    send_and_sync::<TracingGuard>();
}

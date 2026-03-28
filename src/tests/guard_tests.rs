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

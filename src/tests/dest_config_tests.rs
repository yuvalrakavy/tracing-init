use crate::dest_config::DestinationSettings;
use crate::types::{Format, SpanEvents};
use tracing::Level;

#[test]
fn test_wildcard_level_applies_to_all() {
    let mut settings = DestinationSettings::new();
    settings.set_level("*", Level::INFO);
    assert_eq!(settings.resolve_level("console"), Some(Level::INFO));
    assert_eq!(settings.resolve_level("file"), Some(Level::INFO));
    assert_eq!(settings.resolve_level("gelf"), Some(Level::INFO));
    assert_eq!(settings.resolve_level("otel"), Some(Level::INFO));
}

#[test]
fn test_specific_overrides_wildcard() {
    let mut settings = DestinationSettings::new();
    settings.set_level("*", Level::INFO);
    settings.set_level("console", Level::DEBUG);
    assert_eq!(settings.resolve_level("console"), Some(Level::DEBUG));
    assert_eq!(settings.resolve_level("file"), Some(Level::INFO));
}

#[test]
fn test_wildcard_format_applies_to_all() {
    let mut settings = DestinationSettings::new();
    settings.set_format("*", Format::Json);
    assert_eq!(settings.resolve_format("console"), Some(Format::Json));
    assert_eq!(settings.resolve_format("file"), Some(Format::Json));
}

#[test]
fn test_specific_format_overrides_wildcard() {
    let mut settings = DestinationSettings::new();
    settings.set_format("*", Format::Full);
    settings.set_format("console", Format::Pretty);
    assert_eq!(settings.resolve_format("console"), Some(Format::Pretty));
    assert_eq!(settings.resolve_format("file"), Some(Format::Full));
}

#[test]
fn test_filter_resolution() {
    let mut settings = DestinationSettings::new();
    settings.set_filter("*", "my_crate=info");
    settings.set_filter("console", "my_crate=debug,tower=warn");
    assert_eq!(settings.resolve_filter("console"), Some("my_crate=debug,tower=warn"));
    assert_eq!(settings.resolve_filter("file"), Some("my_crate=info"));
}

#[test]
fn test_no_settings_returns_none() {
    let settings = DestinationSettings::new();
    assert_eq!(settings.resolve_level("console"), None);
    assert_eq!(settings.resolve_format("console"), None);
    assert_eq!(settings.resolve_filter("console"), None);
}

#[test]
fn test_bool_settings() {
    let mut settings = DestinationSettings::new();
    settings.set_ansi("console", true);
    settings.set_timestamps("*", true);
    settings.set_timestamps("file", false);
    assert_eq!(settings.resolve_ansi("console"), Some(true));
    assert_eq!(settings.resolve_timestamps("console"), Some(true));
    assert_eq!(settings.resolve_timestamps("file"), Some(false));
}

#[test]
fn test_span_events() {
    let mut settings = DestinationSettings::new();
    settings.set_span_events("console", SpanEvents::NEW | SpanEvents::CLOSE);
    assert_eq!(settings.resolve_span_events("console"), Some(SpanEvents::NEW | SpanEvents::CLOSE));
    assert_eq!(settings.resolve_span_events("file"), None);
}

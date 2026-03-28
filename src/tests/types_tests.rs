use crate::types::{Format, SpanEvents};

#[test]
fn test_format_from_str() {
    assert_eq!("full".parse::<Format>().unwrap(), Format::Full);
    assert_eq!("compact".parse::<Format>().unwrap(), Format::Compact);
    assert_eq!("pretty".parse::<Format>().unwrap(), Format::Pretty);
    assert_eq!("json".parse::<Format>().unwrap(), Format::Json);
    assert!("invalid".parse::<Format>().is_err());
}

#[test]
fn test_format_case_insensitive() {
    assert_eq!("JSON".parse::<Format>().unwrap(), Format::Json);
    assert_eq!("Pretty".parse::<Format>().unwrap(), Format::Pretty);
}

#[test]
fn test_span_events_from_str_single() {
    assert_eq!("new".parse::<SpanEvents>().unwrap(), SpanEvents::NEW);
    assert_eq!("close".parse::<SpanEvents>().unwrap(), SpanEvents::CLOSE);
    assert_eq!("active".parse::<SpanEvents>().unwrap(), SpanEvents::ACTIVE);
    assert_eq!("none".parse::<SpanEvents>().unwrap(), SpanEvents::NONE);
    assert_eq!("all".parse::<SpanEvents>().unwrap(), SpanEvents::ALL);
}

#[test]
fn test_span_events_from_str_combined() {
    let events: SpanEvents = "new,close".parse().unwrap();
    assert_eq!(events, SpanEvents::NEW | SpanEvents::CLOSE);
}

#[test]
fn test_span_events_from_str_with_spaces() {
    let events: SpanEvents = "new, close".parse().unwrap();
    assert_eq!(events, SpanEvents::NEW | SpanEvents::CLOSE);
}

#[test]
fn test_span_events_from_str_invalid() {
    assert!("invalid".parse::<SpanEvents>().is_err());
}

#[cfg(feature = "otel")]
#[test]
fn test_transport_from_str() {
    use crate::types::Transport;
    assert_eq!("http".parse::<Transport>().unwrap(), Transport::Http);
    assert_eq!("grpc".parse::<Transport>().unwrap(), Transport::Grpc);
    assert!("websocket".parse::<Transport>().is_err());
}

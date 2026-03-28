use crate::config::{apply_destination_modifier, discover_config, LoggingConfig};

#[test]
fn test_absolute_destination() {
    assert_eq!(apply_destination_modifier(Some("cgf"), "cg"), "cg");
}

#[test]
fn test_remove_modifier() {
    assert_eq!(apply_destination_modifier(Some("cgf"), "-f"), "cg");
}

#[test]
fn test_add_modifier() {
    assert_eq!(apply_destination_modifier(Some("c"), "+g"), "cg");
}

#[test]
fn test_combined_modifier() {
    assert_eq!(apply_destination_modifier(Some("cgf"), "-f+o"), "cgo");
}

#[test]
fn test_add_otel_modifier() {
    assert_eq!(apply_destination_modifier(Some("cg"), "+o"), "cgo");
}

#[test]
fn test_parse_nested_base() {
    let toml_str = r#"
[logging]
destination = "cg"
level = "info"
service_name = "my-service"
filter = "my_crate=debug"

[logging.console]
level = "debug"
format = "pretty"
ansi = true

[logging.file]
path = "logs"
prefix = "myapp"
rotation = "d:3"
format = "json"

[logging.gelf]
address = "localhost:12201"
level = "warn"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.destination.as_deref(), Some("cg"));
    assert_eq!(config.level.as_deref(), Some("info"));
    assert_eq!(config.service_name.as_deref(), Some("my-service"));
    assert_eq!(config.filter.as_deref(), Some("my_crate=debug"));

    let console = config.console.unwrap();
    assert_eq!(console.level.as_deref(), Some("debug"));
    assert_eq!(console.format.as_deref(), Some("pretty"));
    assert_eq!(console.ansi, Some(true));

    let file = config.file.unwrap();
    assert_eq!(file.path.as_deref(), Some("logs"));
    assert_eq!(file.prefix.as_deref(), Some("myapp"));
    assert_eq!(file.rotation.as_deref(), Some("d:3"));
    assert_eq!(file.format.as_deref(), Some("json"));

    let gelf = config.gelf.unwrap();
    assert_eq!(gelf.address.as_deref(), Some("localhost:12201"));
    assert_eq!(gelf.level.as_deref(), Some("warn"));
}

#[test]
fn test_parse_app_override_with_destination_sections() {
    let toml_str = r#"
[logging]
level = "info"

[logging.console]
format = "pretty"

[logging.myapp]
destination = "co"
level = "debug"

[logging.myapp.console]
format = "json"
level = "trace"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.level.as_deref(), Some("debug"));
    assert_eq!(config.destination.as_deref(), Some("co"));

    let console = config.console.unwrap();
    assert_eq!(console.format.as_deref(), Some("json"));
    assert_eq!(console.level.as_deref(), Some("trace"));
}

#[test]
fn test_inheritance_chain() {
    let toml_str = r#"
[logging]
level = "info"
filter = "base_filter"

[logging.console]
format = "pretty"
filter = "console_filter"

[logging.myapp]
level = "debug"

[logging.myapp.console]
format = "json"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    let console = config.console.unwrap();
    assert_eq!(console.format.as_deref(), Some("json"));
    assert_eq!(console.level, None);
    assert_eq!(console.filter.as_deref(), Some("console_filter"));
    assert_eq!(config.level.as_deref(), Some("debug"));
}

#[test]
fn test_per_app_destination_modifier() {
    let toml_str = r#"
[logging]
destination = "cg"

[logging.myapp]
destination = "-g+f"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.destination.as_deref(), Some("cf"));
}

#[test]
fn test_otel_config() {
    let toml_str = r#"
[logging]
destination = "co"

[logging.otel]
endpoint = "http://localhost:4318"
transport = "http"
level = "error"

[logging.otel.resource]
"service.version" = "1.2.3"
"deployment.environment" = "staging"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    let otel = config.otel.unwrap();
    assert_eq!(otel.endpoint.as_deref(), Some("http://localhost:4318"));
    assert_eq!(otel.transport.as_deref(), Some("http"));
    assert_eq!(otel.level.as_deref(), Some("error"));

    let resource = otel.resource.unwrap();
    assert_eq!(resource.get("service.version").and_then(|v| v.as_str()), Some("1.2.3"));
    assert_eq!(resource.get("deployment.environment").and_then(|v| v.as_str()), Some("staging"));
}

#[test]
fn test_no_logging_section() {
    let toml_str = r#"
[other]
key = "value"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    assert!(LoggingConfig::from_toml(&value, "myapp").is_none());
}

// --- discover_config tests (updated for new LoggingConfig type) ---

#[test]
fn test_discover_from_explicit_file() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("server.toml");
    std::fs::write(&config_path, r#"
[logging]
destination = "cs"
"#).unwrap();

    let (config, source) = discover_config(Some(config_path.to_str().unwrap()), "myapp", false).unwrap();
    assert_eq!(config.destination.as_deref(), Some("cs"));
    assert!(source.contains("server.toml"));
}

#[test]
fn test_discover_fallback_to_logging_toml() {
    let dir = tempfile::tempdir().unwrap();
    let server_path = dir.path().join("server.toml");
    std::fs::write(&server_path, r#"
[other]
key = "value"
"#).unwrap();

    let logging_path = dir.path().join("logging.toml");
    std::fs::write(&logging_path, r#"
[logging]
destination = "f"
"#).unwrap();

    let (config, source) = discover_config(Some(server_path.to_str().unwrap()), "myapp", false).unwrap();
    assert_eq!(config.destination.as_deref(), Some("f"));
    assert!(source.contains("logging.toml"));
    assert!(source.contains("fallback"));
}

#[test]
fn test_discover_no_fallback_when_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let server_path = dir.path().join("server.toml");
    std::fs::write(&server_path, "[other]\nkey = \"value\"\n").unwrap();
    let logging_path = dir.path().join("logging.toml");
    std::fs::write(&logging_path, "[logging]\ndestination = \"f\"\n").unwrap();

    let result = discover_config(Some(server_path.to_str().unwrap()), "myapp", true);
    assert!(result.is_none());
}

#[test]
fn test_discover_auto_logging_toml() {
    let dir = tempfile::tempdir().unwrap();
    let logging_path = dir.path().join("logging.toml");
    std::fs::write(&logging_path, "[logging]\ndestination = \"cs\"\n").unwrap();

    let (config, _source) = discover_config(Some(logging_path.to_str().unwrap()), "myapp", false).unwrap();
    assert_eq!(config.destination.as_deref(), Some("cs"));
}

#[test]
fn test_discover_missing_file_no_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("nonexistent.toml");

    let result = discover_config(Some(missing_path.to_str().unwrap()), "myapp", true);
    assert!(result.is_none());
}

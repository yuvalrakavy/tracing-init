use crate::config::{apply_destination_modifier, discover_config, LoggingConfig};

#[test]
fn test_absolute_destination() {
    assert_eq!(apply_destination_modifier(Some("csf"), "cs"), "cs");
}

#[test]
fn test_absolute_destination_no_base() {
    assert_eq!(apply_destination_modifier(None, "cs"), "cs");
}

#[test]
fn test_remove_modifier() {
    assert_eq!(apply_destination_modifier(Some("csf"), "-f"), "cs");
}

#[test]
fn test_add_modifier() {
    assert_eq!(apply_destination_modifier(Some("c"), "+s"), "cs");
}

#[test]
fn test_combined_modifier() {
    assert_eq!(apply_destination_modifier(Some("csf"), "-f+s"), "cs");
}

#[test]
fn test_add_already_present() {
    assert_eq!(apply_destination_modifier(Some("cs"), "+s"), "cs");
}

#[test]
fn test_remove_not_present() {
    assert_eq!(apply_destination_modifier(Some("cs"), "-f"), "cs");
}

#[test]
fn test_modifier_on_none_is_noop() {
    assert_eq!(apply_destination_modifier(None, "-f"), "");
}

#[test]
fn test_modifier_on_empty_is_noop() {
    assert_eq!(apply_destination_modifier(Some(""), "-f+s"), "s");
}

#[test]
fn test_parse_base_only() {
    let toml_str = r#"
[logging]
destination = "cs"
level = "debug"
server = "localhost:12201"
file_path = "logs"
file_prefix = "myapp"
file_rotation = "d:3"
filter = "myapp=trace"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.destination.as_deref(), Some("cs"));
    assert_eq!(config.level.as_deref(), Some("debug"));
    assert_eq!(config.server.as_deref(), Some("localhost:12201"));
    assert_eq!(config.file_path.as_deref(), Some("logs"));
    assert_eq!(config.file_prefix.as_deref(), Some("myapp"));
    assert_eq!(config.file_rotation.as_deref(), Some("d:3"));
    assert_eq!(config.filter.as_deref(), Some("myapp=trace"));
}

#[test]
fn test_app_overrides_base() {
    let toml_str = r#"
[logging]
destination = "csf"
level = "info"
server = "localhost:12201"

[logging.myapp]
level = "debug"
filter = "myapp=trace"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.destination.as_deref(), Some("csf"));
    assert_eq!(config.level.as_deref(), Some("debug"));
    assert_eq!(config.server.as_deref(), Some("localhost:12201"));
    assert_eq!(config.filter.as_deref(), Some("myapp=trace"));
}

#[test]
fn test_app_destination_modifier() {
    let toml_str = r#"
[logging]
destination = "csf"

[logging.myapp]
destination = "-f+s"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.destination.as_deref(), Some("cs"));
}

#[test]
fn test_file_prefix_not_inherited() {
    let toml_str = r#"
[logging]
destination = "csf"
file_prefix = "shared_prefix"

[logging.myapp]
level = "debug"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    // file_prefix from base is NOT inherited by per-app section
    assert_eq!(config.file_prefix, None);
}

#[test]
fn test_file_prefix_explicit_in_app() {
    let toml_str = r#"
[logging]
destination = "csf"
file_prefix = "shared_prefix"

[logging.myapp]
file_prefix = "custom_prefix"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    assert_eq!(config.file_prefix.as_deref(), Some("custom_prefix"));
}

#[test]
fn test_field_clearing() {
    let toml_str = r#"
[logging]
destination = "csf"
server = "localhost:12201"

[logging.myapp]
server = ""
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    // Empty string clears the inherited value
    assert_eq!(config.server, None);
}

#[test]
fn test_no_logging_section() {
    let toml_str = r#"
[other]
key = "value"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp");

    assert!(config.is_none());
}

#[test]
fn test_base_only_no_app_section() {
    let toml_str = r#"
[logging]
destination = "cs"
file_prefix = "base_prefix"
"#;
    let value: toml::Value = toml_str.parse().unwrap();
    let config = LoggingConfig::from_toml(&value, "myapp").unwrap();

    // With no app section, base file_prefix is used as-is
    assert_eq!(config.destination.as_deref(), Some("cs"));
    assert_eq!(config.file_prefix.as_deref(), Some("base_prefix"));
}

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

    let (config, source) = discover_config(Some(logging_path.to_str().unwrap()), "myapp", false).unwrap();
    assert_eq!(config.destination.as_deref(), Some("cs"));
}

#[test]
fn test_discover_missing_file_no_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let missing_path = dir.path().join("nonexistent.toml");

    let result = discover_config(Some(missing_path.to_str().unwrap()), "myapp", true);
    assert!(result.is_none());
}

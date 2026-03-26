use crate::TracingInit;

#[test]
fn test_config_file_and_config_toml_both_panics() {
    let toml_str = "[logging]\ndestination = \"c\"\n";
    let value: toml::Value = toml_str.parse().unwrap();

    let result = std::panic::catch_unwind(|| {
        let mut builder = TracingInit::builder("test");
        builder.config_file("test.toml").config_toml(&value);
    });

    assert!(result.is_err());
}

#[test]
fn test_config_toml_and_config_file_both_panics() {
    let toml_str = "[logging]\ndestination = \"c\"\n";
    let value: toml::Value = toml_str.parse().unwrap();

    let result = std::panic::catch_unwind(|| {
        let mut builder = TracingInit::builder("test");
        builder.config_toml(&value).config_file("test.toml");
    });

    assert!(result.is_err());
}

#[test]
fn test_builder_defaults() {
    let mut builder = TracingInit::builder("testapp");
    builder
        .no_auto_config_file()
        .ignore_environment_variables();

    let debug = format!("{:?}", builder);
    assert!(debug.contains("testapp"));
    assert!(debug.contains("no_auto_config_file: true"));
    assert!(debug.contains("ignore_env_vars: true"));
}

use tracing::{event, Level};

#[test]
#[ignore] // Global subscriber can only be set once per process
fn test_full_logging() {
    let summary = TracingInit::builder("App")
        .log_to_console(true)
        .log_to_file(true)
        .log_to_server(true)
        .no_auto_config_file()
        .init()
        .unwrap();

    println!("{summary}");
    event!(Level::INFO, "test");
}

#[test]
#[ignore] // Global subscriber can only be set once per process
fn test_default_logging() {
    let summary = TracingInit::builder("App")
        .no_auto_config_file()
        .init()
        .unwrap();

    println!("{summary}");
    event!(Level::INFO, "test");
}

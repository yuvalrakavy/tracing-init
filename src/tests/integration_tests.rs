use crate::TracingInit;
use crate::types::Format;
use tracing::Level;

#[cfg(feature = "config")]
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

#[cfg(feature = "config")]
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
fn test_builder_destination_keyed_api() {
    let mut builder = TracingInit::builder("testapp");
    builder
        .destination("cg")
        .level("*", Level::INFO)
        .level("console", Level::DEBUG)
        .format("console", Format::Pretty)
        .no_auto_config_file()
        .ignore_environment_variables();
    let debug = format!("{:?}", builder);
    assert!(debug.contains("testapp"));
}

#[test]
fn test_log_to_legacy_methods() {
    let mut builder = TracingInit::builder("testapp");
    builder
        .log_to_console(true)
        .log_to_file(true)
        .log_to_gelf_server(false)
        .no_auto_config_file()
        .ignore_environment_variables();
    let debug = format!("{:?}", builder);
    assert!(debug.contains("testapp"));
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

#[test]
#[ignore] // Global subscriber can only be set once per process
fn test_full_logging_new_api() {
    use tracing::{event, Level};
    let guard = TracingInit::builder("App")
        .destination("c")
        .level("*", Level::INFO)
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .unwrap();
    println!("Logging: {guard}");
    event!(Level::INFO, "test with new API");
    drop(guard);
}

#[test]
#[ignore] // Global subscriber can only be set once per process
fn test_default_logging() {
    use tracing::{event, Level};
    let guard = TracingInit::builder("App")
        .no_auto_config_file()
        .ignore_environment_variables()
        .init()
        .unwrap();
    println!("Logging: {guard}");
    event!(Level::INFO, "test");
    drop(guard);
}

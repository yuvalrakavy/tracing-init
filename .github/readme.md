# tracing-init

Rust tracing framework is excellent but a bit complex. This crate enables you to initialize it with one line of code.

For example:

```rust
        TracingInit::builder("App")
            .log_to_console(true)
            .log_to_file(true)
            .log_to_server(true)
            .init()
            .unwrap();

```

It handles the most common cases:

* logging to console
* logging to file
* logging to server, for example [graylog](https://graylog.org/), or [grafana loki](https://grafana.com/oss/loki/) (using GELF format)


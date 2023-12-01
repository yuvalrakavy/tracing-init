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

In is (also) possible to specify the values via environment variables:

 It is possible to specify the values of the tracing subscriber using environment variables:

* LOG_DESTINATION - the value should contain one or more of the following characters: 'c' - console, 'f' - file, 's' - server
* LOG_FILE_PATH - the path to the log file
* LOG_FILE_ROTATION - the rotation of the log file. The value should be in the format:
  *rotation*(:*count*) where *rotation* is one of the following: d - daily, h - hourly, m - minutely, n - never and *count* is the number of backups to keep
* LOG_SERVER - the address of the logging server in the format \<host\>:\<port\>
* LOG_LEVEL - the log level for the tracing subscriber (can be one of: error, warn, info, debug, trace)
* RUST_LOG - logging filter (see [filter setting](https://docs.rs/tracing-subscriber/0.2.14/tracing_subscriber/filter/struct.EnvFilter.html#filter-syntax) for details)

So the above example can be simplified to:

```rust
        TracingInit::builder("App")
            .init()
            .unwrap();

```

and to log to console and to a log file, run the application with the following environment variables:

```LOG_DESTINATION=cf app```

The logs will be written to the console and to the file app.log in the current directory.

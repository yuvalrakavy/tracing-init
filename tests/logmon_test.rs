#![cfg(feature = "logmon-test")]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, UdpSocket};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing_init::logmon_test;

#[logmon_test]
fn sync_child() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("LOGMON_CHILD_UNCONFIGURED").is_some() {
        // A local cargo test must be free to install its own collector.
        tracing::subscriber::set_global_default(tracing_subscriber::registry())?;
    }
    if std::env::var_os("LOGMON_CHILD_PANIC").is_some() {
        tracing::info!("before panic");
        panic!("intentional fixture failure");
    }
    Ok(())
}

#[logmon_test(flavor = "multi_thread", worker_threads = 2)]
async fn async_child() {
    std::thread::spawn(|| {
        tracing::info!(test_attempt_id = "incorrect", "thread without parent");
        let _span = tracing::info_span!(parent: None, "detached-thread").entered();
    })
    .join()
    .unwrap();
    tokio::spawn(async {
        tracing::info!("task without parent");
        let _span = tracing::info_span!(parent: None, "detached-task").entered();
    })
    .await
    .unwrap();
}

#[logmon_test]
#[ignore = "attribute preservation fixture"]
#[should_panic(expected = "fixture")]
fn preserves_libtest_attributes() {
    panic!("fixture");
}

const ID: &str = "0123456789abcdeffedcba9876543210";

fn capture(test: &str, panic: bool) -> (Vec<serde_json::Value>, Vec<serde_json::Value>) {
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", tcp.local_addr().unwrap());
    tcp.set_nonblocking(true).unwrap();
    let done = Arc::new(AtomicBool::new(false));
    let stop = done.clone();
    let collector = std::thread::spawn(move || {
        let mut spans = Vec::new();
        loop {
            let mut stream = match tcp.accept() {
                Ok((socket, _)) => socket,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => panic!("{e}"),
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
            for rs in payload["resourceSpans"].as_array().unwrap() {
                for scope in rs["scopeSpans"].as_array().unwrap() {
                    spans.extend(scope["spans"].as_array().unwrap().iter().cloned());
                }
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .unwrap();
        }
        spans
    });
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test])
        .env("LOGMON_TEST_TRACE_ID", ID)
        .env("LOGMON_TEST_OTLP_ENDPOINT", endpoint)
        .env(
            "LOGMON_TEST_GELF_ADDR",
            udp.local_addr().unwrap().to_string(),
        )
        .env_remove("LOGMON_CHILD_UNCONFIGURED")
        .env_remove("RUST_LOG");
    if panic {
        command.env("LOGMON_CHILD_PANIC", "1");
    }
    let output = command.output().unwrap();
    done.store(true, Ordering::Release);
    let spans = collector.join().unwrap();
    assert_eq!(output.status.success(), !panic, "{output:?}");
    let mut logs = Vec::new();
    let mut buf = [0; 65536];
    udp.set_nonblocking(true).unwrap();
    while let Ok(len) = udp.recv(&mut buf) {
        logs.push(serde_json::from_slice(&buf[..len]).unwrap());
    }
    (logs, spans)
}

#[test]
fn nextest_context_survives_detached_threads_async_tasks_and_unwind() {
    for (test, panics, messages, names) in [
        (
            "async_child",
            false,
            &["thread without parent", "task without parent"][..],
            &["rust.test", "detached-thread", "detached-task"][..],
        ),
        (
            "sync_child",
            true,
            &["before panic"][..],
            &["rust.test"][..],
        ),
    ] {
        let (logs, spans) = capture(test, panics);
        for msg in messages {
            let row = logs
                .iter()
                .find(|row| row["short_message"] == *msg)
                .unwrap_or_else(|| panic!("missing {msg}: {logs:?}"));
            assert_eq!(row["_test_attempt_id"], ID);
            assert_eq!(row["_trace_id"], ID);
        }
        for name in names {
            let span = spans
                .iter()
                .find(|s| s["name"] == *name)
                .unwrap_or_else(|| panic!("missing {name}: {spans:?}"));
            assert_eq!(span["traceId"].as_str().unwrap().to_ascii_lowercase(), ID);
            assert!(span["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["key"] == "test_attempt_id" && a["value"]["stringValue"] == ID));
        }
    }
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "sync_child"])
        .env_remove("LOGMON_TEST_TRACE_ID")
        .env_remove("LOGMON_CHILD_PANIC")
        .env("LOGMON_CHILD_UNCONFIGURED", "1")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

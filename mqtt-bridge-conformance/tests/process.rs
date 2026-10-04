//! The bridge process (Bridge Runtime spec §5, §9; the fleet's F1, F3, no-hang 3b round 3): the
//! real binary (`fake-bridge`, the runtime driving the fake device) against `FakeBroker` on
//! 127.0.0.1 — never the shared broker — with its logging in a directory of the test's own (GELF
//! to the discard port on loopback, nothing to logmon).

use std::io::{ErrorKind, Write};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use mqtt_bridge_conformance::*;

const INSTANCE: &str = "proc";

/// A working directory of the test's own, with a `logging.toml` that keeps the bridge's logs in a
/// file there.
fn workdir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("process-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the test's working directory");
    std::fs::write(dir.join("logging.toml"), "[logging]\non_destination_error = \"skip\"\n\n[logging.gelf]\naddress = \"127.0.0.1:9\"\n")
        .expect("the test's logging.toml");
    dir
}

/// Everything the bridge wrote to its log files in `dir`.
fn logged(dir: &Path) -> String {
    let mut text = String::new();
    if let Ok(entries) = std::fs::read_dir(dir.join("logs")) {
        for entry in entries.flatten() {
            text.push_str(&std::fs::read_to_string(entry.path()).unwrap_or_default());
        }
    }
    text
}

fn bridge(dir: &Path, broker: &FakeBroker, extra: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-bridge"));
    command
        .args(["--instance", INSTANCE, "--broker", broker.address().as_str(), "--root", "Proc", "--log-config"])
        .arg(dir.join("logging.toml"))
        .args(extra)
        .current_dir(dir)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    command
}

fn signal(child: &tokio::process::Child, signal: &str) {
    let pid = child.id().expect("the bridge's pid").to_string();
    let sent = std::process::Command::new("kill").args([signal, &pid]).status();
    assert!(sent.is_ok_and(|s| s.success()), "could not send {signal} to the bridge");
}

/// The child's exit within `bound`, or `None` (it is then killed).
async fn exit_within(child: &mut tokio::process::Child, bound: Duration) -> Option<std::process::ExitStatus> {
    match tokio::time::timeout(bound, child.wait()).await {
        Ok(status) => Some(status.expect("the bridge's exit status")),
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            None
        }
    }
}

async fn stops_cleanly_on(sig: &str, name: &str) {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir(name);
    let mut child = bridge(&dir, &broker, &[]).stdout(Stdio::null()).stderr(Stdio::null()).spawn().expect("start the bridge");
    let active = format!("Proc/Active/{INSTANCE}");
    assert!(
        broker.wait_until(BOUND, |b| b.received_on(&active).iter().any(|r| &r.payload[..] == b"true")).await,
        "the bridge never announced itself"
    );
    signal(&child, sig);
    let status = exit_within(&mut child, Duration::from_secs(15)).await;
    let log = logged(&dir);
    assert!(status.is_some_and(|s| s.success()), "the bridge did not stop cleanly on {sig}: {status:?}; the log:\n{log}");
    assert!(log.contains("Bridge stopped"), "the bridge exited without its bounded shutdown; the log:\n{log}");
    let last_active = broker.received_on(&active).last().map(|r| r.payload.clone());
    assert_eq!(last_active.as_deref(), Some(&b"false"[..]), "the bridge did not say Active=false on its way out");
    assert!(broker.events().iter().any(|e| matches!(e, BrokerEvent::Disconnected)), "the bridge did not DISCONNECT");
}

/// SIGTERM — `kill`, systemd — stops the bridge through its bounded shutdown: `Active=false`,
/// DISCONNECT, exit 0, the log flushed.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_stops_the_bridge_cleanly() {
    stops_cleanly_on("-TERM", "sigterm").await;
}

/// SIGINT the same.
#[tokio::test(flavor = "multi_thread")]
async fn sigint_stops_the_bridge_cleanly() {
    stops_cleanly_on("-INT", "sigint").await;
}

/// SIGTERM during a device connect that does not return (its bound set to a minute) stops the
/// bridge within its bound: the stop never waits on the device.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_a_stalled_connect_stops_the_bridge_within_its_bound() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("stalled_connect");
    let mut child = bridge(&dir, &broker, &["--device", "stall-connect", "--connect-timeout", "60s"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the bridge");
    assert!(broker.wait_for_subscription(&format!("Proc/Command/{INSTANCE}"), BOUND).await, "the bridge never subscribed");
    let status_topic = format!("Proc/Status/{INSTANCE}");
    assert!(
        broker.wait_until(BOUND, |b| b.received_on(&status_topic).iter().any(|r| &r.payload[..] == b"connecting")).await,
        "the bridge never said it was connecting"
    );
    signal(&child, "-TERM");
    let signalled = Instant::now();
    let status = exit_within(&mut child, Duration::from_secs(15)).await;
    let took = signalled.elapsed();
    assert!(
        status.is_some_and(|s| s.success()) && took < Duration::from_secs(4),
        "the bridge did not stop within its bound during a stalled connect: {status:?}, {took:?} after SIGTERM; the log:\n{}",
        logged(&dir)
    );
}

/// `O_NONBLOCK`, for opening the gate FIFO's write end without waiting for its reader.
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(not(target_os = "macos"))]
const O_NONBLOCK: i32 = 0o4000;
/// `ENXIO`: a FIFO opened for writing without waiting has no reader yet.
const ENXIO: i32 = 6;

/// The whole process stops within its bound, whatever a blocking thread is doing (the fleet's F1
/// and F3). The logging start is held through the debug build's seam
/// (`MQTT_BRIDGE_TEST_LOGGING_GATE`): it reads a FIFO whose write end the test holds open and never
/// writes. The test proves the read began (the write end opens, without waiting, only once a reader
/// is there), sends SIGTERM, and lets the FIFO go only after the bridge has exited. It must exit
/// cleanly within 3 s: the stop signals are taken before the logging starts (after it, SIGTERM
/// kills the held bridge); the start is raced against the stop (not raced, it never gets past the
/// start); the runtime is shut down within its grace (dropped, it waits for the held thread for
/// good). A regression never exits while the FIFO is held, so no delay can let it pass.
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_during_a_held_logging_start_stops_the_bridge_within_its_bound() {
    use std::os::unix::fs::OpenOptionsExt;

    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("held_start");
    let fifo = dir.join("logging-gate");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(made.is_ok_and(|s| s.success()), "could not make the FIFO");
    let mut child = bridge(&dir, &broker, &[])
        .env(mqtt_bridge_kit::logging::TEST_LOGGING_GATE, &fifo)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the bridge");
    let deadline = Instant::now() + Duration::from_secs(10);
    let gate = loop {
        match std::fs::OpenOptions::new().write(true).custom_flags(O_NONBLOCK).open(&fifo) {
            Ok(gate) => break gate,
            Err(e) if e.raw_os_error() == Some(ENXIO) && Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => panic!("the bridge's logging start never began reading its gate: {e}"),
        }
    };
    signal(&child, "-TERM");
    let signalled = Instant::now();
    let status = exit_within(&mut child, Duration::from_secs(15)).await;
    let took = signalled.elapsed();
    drop(gate);
    assert!(
        status.is_some_and(|s| s.success()) && took < Duration::from_secs(3),
        "the bridge did not stop cleanly within its bound on SIGTERM while its logging start was held: {status:?}, {took:?} after the signal"
    );
    assert_eq!(broker.connections(), 0, "the bridge reached its service, so its logging start was not held and this test proves nothing");
}

/// A stream whose buffer is already full, as a supervisor's stdout that stopped draining. Returns
/// the full end, for the bridge, and the other, to hold unread until the bridge has exited.
fn full_stream() -> (UnixStream, UnixStream) {
    let (full, unread) = UnixStream::pair().expect("a socket pair");
    full.set_nonblocking(true).expect("a non-blocking socket");
    for chunk in [&[b'x'; 4096][..], &b"x"[..]] {
        loop {
            match (&full).write(chunk) {
                Ok(_) => continue,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) => panic!("filling the socket: {e}"),
            }
        }
    }
    full.set_nonblocking(false).expect("a blocking socket again");
    (full, unread)
}

/// The bridge's stdout and stderr are a pipe nobody drains: it must start — announce itself — and
/// stop on SIGTERM, with both full and unread throughout. A line it wrote there itself would wait
/// outside every bound.
#[tokio::test(flavor = "multi_thread")]
async fn a_stdout_and_stderr_nobody_reads_hold_neither_the_start_nor_the_stop() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("full_stdout");
    let (full, unread) = full_stream();
    let stdout = OwnedFd::from(full.try_clone().expect("a second handle on the full socket"));
    let mut child =
        bridge(&dir, &broker, &[]).stdout(Stdio::from(stdout)).stderr(Stdio::from(OwnedFd::from(full))).spawn().expect("start the bridge");
    let active = format!("Proc/Active/{INSTANCE}");
    let started = broker.wait_until(Duration::from_secs(10), |b| b.received_on(&active).iter().any(|r| &r.payload[..] == b"true")).await;
    signal(&child, "-TERM");
    let status = exit_within(&mut child, Duration::from_secs(15)).await;
    drop(unread);
    assert!(started, "the bridge never announced itself: its start waited on a stdout or stderr nobody reads");
    assert!(
        status.is_some_and(|s| s.success()),
        "the bridge did not stop cleanly on SIGTERM with a stdout and stderr nobody reads: {status:?}"
    );
}

/// A driver's panic with a stdout and stderr nobody drains still ends the bridge with a failure
/// status (review M2). The default panic hook writes its report on the panicking thread: with the
/// pipe full that write never returns, the worker never reaches its `catch_unwind`, and nobody
/// sees the death.
#[tokio::test(flavor = "multi_thread")]
async fn a_driver_panic_with_a_stderr_nobody_reads_ends_the_bridge_with_a_failure() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("panic_apply");
    let (full, unread) = full_stream();
    let stdout = OwnedFd::from(full.try_clone().expect("a second handle on the full socket"));
    let mut child = bridge(&dir, &broker, &["--device", "panic-apply"])
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(OwnedFd::from(full)))
        .spawn()
        .expect("start the bridge");
    let status_topic = format!("Proc/Status/{INSTANCE}");
    let up = broker.wait_until(BOUND, |b| b.received_on(&status_topic).iter().any(|r| &r.payload[..] == b"connected")).await;
    assert!(broker.send_with(&format!("Proc/Desired/{INSTANCE}/t1/power"), "false", &SendOptions::default()) || up);
    let status = exit_within(&mut child, Duration::from_secs(10)).await;
    drop(unread);
    assert!(up, "the device never connected");
    assert!(
        status.is_some_and(|s| !s.success()),
        "a driver panic with a stderr nobody reads did not end the bridge with a failure: {status:?}; the log:\n{}",
        logged(&dir)
    );
}

/// A panic in the MQTT session's task, with a stdout and stderr nobody drains, ends the bridge
/// with a failure status, and its connection with it — the last will says `Active=false`. Left
/// alive, the pump would go on answering pings, and `Active=true` would stand for a bridge that
/// reads nothing (review M2). The panic comes from the debug build's seam
/// (`MQTT_BRIDGE_TEST_PANIC_TOPIC`).
#[tokio::test(flavor = "multi_thread")]
async fn a_session_panic_with_a_stderr_nobody_reads_ends_the_bridge_and_its_connection() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("panic_session");
    let (full, unread) = full_stream();
    let stdout = OwnedFd::from(full.try_clone().expect("a second handle on the full socket"));
    let poison = format!("Proc/Config/{INSTANCE}/panic");
    let mut child = bridge(&dir, &broker, &[])
        .env(mqtt_bridge_kit::session::TEST_PANIC_TOPIC, &poison)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(OwnedFd::from(full)))
        .spawn()
        .expect("start the bridge");
    let active = format!("Proc/Active/{INSTANCE}");
    let up = broker.wait_until(BOUND, |b| b.received_on(&active).iter().any(|r| &r.payload[..] == b"true")).await;
    assert!(broker.send(&poison, "{}") || up);
    let status = exit_within(&mut child, Duration::from_secs(10)).await;
    drop(unread);
    assert!(up, "the bridge never announced itself");
    assert!(
        status.is_some_and(|s| !s.success()),
        "a session panic with a stderr nobody reads did not end the bridge with a failure: {status:?}; the log:\n{}",
        logged(&dir)
    );
    assert!(
        broker.wait_until(BOUND, |b| b.retained_on(&active).as_deref() == Some(&b"false"[..])).await,
        "Active=true stood after the session died: {:?}",
        broker.retained_on(&active)
    );
}

/// A logging start that fails says so on stderr — the one message the bridge writes there itself —
/// and runs without a log; with a stderr nobody drains that line must hold neither the start nor
/// the stop (review MINOR 1: it was written on the start's thread, which the start waits for). The
/// failure comes from the debug build's seam: a gate file that says `fail`.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_logging_start_with_a_stderr_nobody_reads_holds_neither_the_start_nor_the_stop() {
    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("failed_start");
    let gate = dir.join("logging-gate");
    std::fs::write(&gate, "fail").expect("the gate file");
    let (full, unread) = full_stream();
    let stdout = OwnedFd::from(full.try_clone().expect("a second handle on the full socket"));
    let mut child = bridge(&dir, &broker, &[])
        .env(mqtt_bridge_kit::logging::TEST_LOGGING_GATE, &gate)
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(OwnedFd::from(full)))
        .spawn()
        .expect("start the bridge");
    let active = format!("Proc/Active/{INSTANCE}");
    let started = broker.wait_until(Duration::from_secs(10), |b| b.received_on(&active).iter().any(|r| &r.payload[..] == b"true")).await;
    signal(&child, "-TERM");
    let status = exit_within(&mut child, Duration::from_secs(15)).await;
    drop(unread);
    assert!(started, "the bridge never announced itself: its failed logging start waited on a stderr nobody reads");
    assert!(status.is_some_and(|s| s.success()), "the bridge did not stop cleanly on SIGTERM: {status:?}");
}

/// When the stop signals cannot be taken the bridge does not start — and does not wait without a
/// bound for its logging start first, to say so (review, process.rs:208). The start is held by the
/// debug build's gate (a FIFO nobody writes); the signals are refused by its seam.
#[tokio::test(flavor = "multi_thread")]
async fn without_its_stop_signals_the_bridge_ends_within_a_bound_whatever_its_logging_start_does() {
    use std::os::unix::fs::OpenOptionsExt;

    let broker = FakeBroker::start().await;
    broker.cap_connections(20);
    let dir = workdir("no_signals");
    let fifo = dir.join("logging-gate");
    let made = std::process::Command::new("mkfifo").arg(&fifo).status();
    assert!(made.is_ok_and(|s| s.success()), "could not make the FIFO");
    let mut child = bridge(&dir, &broker, &[])
        .env(mqtt_bridge_kit::logging::TEST_LOGGING_GATE, &fifo)
        .env(mqtt_bridge_kit::process::TEST_NO_SIGNALS, "1")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the bridge");
    let deadline = Instant::now() + Duration::from_secs(10);
    let gate = loop {
        match std::fs::OpenOptions::new().write(true).custom_flags(O_NONBLOCK).open(&fifo) {
            Ok(gate) => break gate,
            Err(e) if e.raw_os_error() == Some(ENXIO) && Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(10)).await,
            Err(e) => panic!("the bridge's logging start never began reading its gate: {e}"),
        }
    };
    let status = exit_within(&mut child, Duration::from_secs(12)).await;
    drop(gate);
    assert!(
        status.is_some_and(|s| !s.success()),
        "without its stop signals the bridge did not end with a failure within its bound while its logging start was held: {status:?}"
    );
    assert_eq!(broker.connections(), 0, "the bridge started without its stop signals");
}

/// A usage error is said on stderr — the command line's own output, before anything has started —
/// with exit status 2.
#[tokio::test(flavor = "multi_thread")]
async fn a_usage_error_is_said_and_ends_with_status_2() {
    let output =
        tokio::process::Command::new(env!("CARGO_BIN_EXE_fake-bridge")).args(["--broker", "127.0.0.1:9"]).stdin(Stdio::null()).output();
    let output = tokio::time::timeout(BOUND, output).await.expect("the bridge did not exit on a usage error").expect("run the bridge");
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--instance"), "{}", String::from_utf8_lossy(&output.stderr));
}

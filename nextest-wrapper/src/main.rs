mod lifecycle;

use chrono::Utc;
use lifecycle::Run;
use serde_json::json;
use std::{
    collections::VecDeque,
    env,
    ffi::OsString,
    io::{self, Write},
    os::unix::process::{CommandExt, ExitStatusExt},
    process::{Command, ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command as AsyncCommand,
    signal::unix::{signal, SignalKind},
};

const TAIL_BYTES: usize = 8192;

#[derive(Default)]
struct Output {
    tail: VecDeque<u8>,
    bytes: u64,
}
impl Output {
    fn push(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len() as u64);
        let incoming = &bytes[bytes.len().saturating_sub(TAIL_BYTES)..];
        let discard = (self.tail.len() + incoming.len()).saturating_sub(TAIL_BYTES);
        self.tail.drain(..discard);
        self.tail.extend(incoming);
    }
    fn text(&mut self) -> String {
        String::from_utf8_lossy(self.tail.make_contiguous()).into_owned()
    }
}

async fn copy<R: AsyncRead + Unpin>(
    pipe: &mut R,
    out: &mut Output,
    stderr: bool,
) -> io::Result<()> {
    let mut buf = [0u8; 4096];
    loop {
        let n = pipe.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        let bytes = &buf[..n];
        out.push(bytes);
        if stderr {
            let _ = io::stderr().write_all(bytes);
        } else {
            let mut dest = io::stdout().lock();
            let _ = dest.write_all(bytes);
            let _ = dest.flush();
        }
    }
}

// A descendant can retain inherited stdio after the test exits. Give nextest
// the same open descriptor to observe for its leak-timeout policy. Helpers are
// spawned only for streams which did not close promptly.
fn relay(pipe: Stdio, stderr: bool) {
    let Ok(exe) = env::current_exe() else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg(if stderr {
        "--relay-stderr"
    } else {
        "--relay-stdout"
    })
    .stdin(pipe);
    if stderr {
        cmd.stdout(Stdio::null());
    } else {
        cmd.stderr(Stdio::null());
    }
    let _ = cmd.spawn();
}

async fn wait_child(child: &mut tokio::process::Child) -> io::Result<ExitStatus> {
    let mut term = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut quit = signal(SignalKind::quit())?;
    loop {
        let sig = tokio::select! {
            res=child.wait()=>return res,
            _=term.recv()=>libc::SIGTERM,
            _=interrupt.recv()=>libc::SIGINT,
            _=hangup.recv()=>libc::SIGHUP,
            _=quit.recv()=>libc::SIGQUIT,
        };
        if let Some(id) = child.id() {
            unsafe {
                libc::kill(id as libc::pid_t, sig);
            }
        }
    }
}

async fn wrap(mut run: Run, argv: Vec<OsString>) -> io::Result<ExitStatus> {
    let id = uuid::Uuid::new_v4().simple().to_string();
    let field = match env::var("LOGMON_TEST_SELECTOR").as_deref() {
        Ok("trace_id") => "trace_id",
        _ => "test_attempt_id",
    };
    let mut event = json!({"run_id":run.id,"execution_id":id,"name":run.name,
        "selector":{"field":field,"value":id},"started_at":Utc::now(),"rerun_of":run.rerun});
    let mut start_event = event.clone();
    start_event["estimated_ms"] = json!(run.estimated_ms);
    start_event["test_timeout_ms"] = json!(run.test_timeout_ms);
    run.report(true, &start_event).await;
    let mut command = AsyncCommand::new(&argv[0]);
    command
        .args(&argv[1..])
        .env("LOGMON_TEST_TRACE_ID", &id)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            event["ended_at"] = json!(Utc::now());
            event["verdict"] = "ABORTED".into();
            event["fields"] = json!({"message":e.to_string(),"failed_test":run.name});
            run.report(false, &event).await;
            return Err(e);
        }
    };
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out = Output::default();
    let mut err = Output::default();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let status;
    {
        let out_future = copy(&mut stdout, &mut out, false);
        let err_future = copy(&mut stderr, &mut err, true);
        tokio::pin!(out_future, err_future);
        let wait = wait_child(&mut child);
        tokio::pin!(wait);
        loop {
            tokio::select! {
                s=&mut wait=>{status=s?;break;},
                _=&mut out_future, if !stdout_done=>stdout_done=true,
                _=&mut err_future, if !stderr_done=>stderr_done=true,
            }
        }
        let until = tokio::time::sleep(Duration::from_millis(10));
        tokio::pin!(until);
        while !stdout_done || !stderr_done {
            tokio::select! {
                _=&mut out_future, if !stdout_done=>stdout_done=true,
                _=&mut err_future, if !stderr_done=>stderr_done=true,
                _=&mut until=>break,
            }
        }
    }
    if !stdout_done {
        if let Ok(p) = stdout.try_into() {
            relay(p, false);
        }
    }
    if !stderr_done {
        if let Ok(p) = stderr.try_into() {
            relay(p, true);
        }
    }
    event["ended_at"] = json!(Utc::now());
    event["verdict"] = json!(if status.success() {
        "OK"
    } else if status.signal().is_some() {
        "ABORTED"
    } else {
        "FAIL"
    });
    if !status.success() {
        let stdout = out.text();
        let stderr = err.text();
        let message = if !stderr.is_empty() {
            stderr.as_str()
        } else if !stdout.is_empty() {
            stdout.as_str()
        } else {
            "test process failed without output"
        };
        event["fields"] = json!({"message":message,"failed_test":run.name,"stdout":stdout,"stderr":stderr,
            "stdout_bytes":out.bytes,"stderr_bytes":err.bytes,
            "stdout_truncated":out.bytes>TAIL_BYTES as u64 || !stdout_done,"stderr_truncated":err.bytes>TAIL_BYTES as u64 || !stderr_done,
            "exit_code":status.code(),"signal":status.signal(),"nextest_attempt_id":env::var("NEXTEST_ATTEMPT_ID").ok()});
    }
    run.report(false, &event).await;
    Ok(status)
}

fn main() {
    let args: Vec<_> = env::args_os().skip(1).collect();
    if args.len() == 1 && (args[0] == "--relay-stdout" || args[0] == "--relay-stderr") {
        if args[0] == "--relay-stderr" {
            let _ = io::copy(&mut io::stdin().lock(), &mut io::stderr().lock());
        } else {
            let _ = io::copy(&mut io::stdin().lock(), &mut io::stdout().lock());
        }
        return;
    }
    if args.is_empty() {
        eprintln!("usage: logmon-nextest-wrapper TEST_BINARY [ARG...]");
        std::process::exit(2);
    }
    if let Some(run) = Run::current() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Ok(rt) = runtime {
            match rt.block_on(wrap(run, args)) {
                Ok(status) => {
                    if let Some(sig) = status.signal() {
                        // Reproduce termination by signal, which nextest distinguishes from an exit code.
                        unsafe {
                            libc::signal(sig, libc::SIG_DFL);
                            libc::raise(sig);
                        }
                        std::process::exit(128 + sig);
                    }
                    std::process::exit(status.code().unwrap_or(1));
                }
                Err(err) => {
                    eprintln!("logmon-nextest-wrapper: {err}");
                    std::process::exit(127);
                }
            }
        }
    }
    let err = Command::new(&args[0]).args(&args[1..]).exec();
    eprintln!("logmon-nextest-wrapper: {err}");
    std::process::exit(127);
}

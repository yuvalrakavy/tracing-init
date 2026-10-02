//! The CLI's verdicts (review finding C-18): a check that read no file, or read files and found no
//! wait, exits 1 — a wrong `--src` is otherwise green — and a clean check of real waits exits 0.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ROWS: &str = "| Key | Kind | Waits on | Argument |\n|---|---|---|---|\n";

/// A fresh crate directory with `files` (relative path → contents).
fn fixture(name: &str, files: &[(&str, &str)]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("wait-lint-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    for (rel, text) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    dir
}

/// Run `wait-lint --root dir --src src --registry registry.md`, killed if it outlives 30 s: its exit
/// code and its stdout (a few lines, well inside a pipe's buffer).
fn check(dir: &Path) -> (i32, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wait-lint"))
        .args(["--root", dir.to_str().unwrap(), "--src", "src", "--registry", "registry.md"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("run wait-lint");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll wait-lint") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("wait-lint did not finish within 30 s");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut out = String::new();
    child.stdout.take().expect("piped stdout").read_to_string(&mut out).expect("read wait-lint's stdout");
    (status.code().unwrap_or(-1), out)
}

#[test]
fn the_cli_refuses_an_empty_scan_and_passes_a_clean_one() {
    let registry = format!("{ROWS}\n```wait-lint-waiters\n```\n");

    let nothing = fixture("nothing", &[("registry.md", &registry)]);
    let (code, out) = check(&nothing);
    assert_eq!(code, 1, "a scan that read no file must fail:\n{out}");
    assert!(out.contains("read 0 production file(s) and found 0 wait(s)"), "{out}");

    let no_wait = fixture("no-wait", &[("registry.md", &registry), ("src/lib.rs", "pub fn f() -> u8 {\n    1\n}\n")]);
    let (code, out) = check(&no_wait);
    assert_eq!(code, 1, "a scan that read files but found no wait must fail:\n{out}");
    assert!(out.contains("read 1 production file(s) and found 0 wait(s)"), "{out}");

    // The refusal is about the scan, not the CLI failing on everything.
    let clean = format!("{ROWS}| `k` | acyclic | a lock | nothing waits back |\n\n```wait-lint-waiters\nk src/lib.rs f\n```\n");
    let waits = "pub async fn f(m: M) {\n    // WAIT: k\n    m.lock().await;\n}\n";
    let ok = fixture("clean", &[("registry.md", &clean), ("src/lib.rs", waits)]);
    let (code, out) = check(&ok);
    assert_eq!(code, 0, "a clean check of real waits passes:\n{out}");

    for dir in [nothing, no_wait, ok] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

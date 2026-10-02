//! `wait-lint --root <dir> --src <dir>… --registry <file> [--write | --list | --summary]`
//!
//! Checks every wait under the `--src` directories against the registry and prints the
//! findings; exits 1 when there are any. `--write` regenerates the registry's waiters block.
//! `--list` prints every wait with its tag; `--summary` counts the waits by kind and by tag.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut root = PathBuf::from(".");
    let mut src = Vec::new();
    let mut registry = None;
    let mut mode = "check";
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => root = PathBuf::from(args.next().expect("--root <dir>")),
            "--src" => src.push(args.next().expect("--src <dir>")),
            "--registry" => registry = Some(args.next().expect("--registry <file>")),
            "--write" => mode = "write",
            "--list" => mode = "list",
            "--summary" => mode = "summary",
            other => {
                eprintln!("unknown argument `{other}`");
                return ExitCode::from(2);
            }
        }
    }
    let Some(registry) = registry else {
        eprintln!("usage: wait-lint --root <dir> --src <dir>... --registry <file> [--write | --list | --summary]");
        return ExitCode::from(2);
    };
    let src: Vec<&str> = src.iter().map(String::as_str).collect();
    if mode == "write" {
        return match wait_lint::write_waiters(&root, &src, &registry) {
            Ok(changed) => {
                println!("{}", if changed { "waiters block rewritten" } else { "waiters block already current" });
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        };
    }
    let report = match wait_lint::check_dirs(&root, &src, &registry) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match mode {
        "list" => {
            for s in &report.sites {
                println!("{}:{}\t{}\t{}\t{}", s.file, s.line, s.func, s.tag.as_deref().unwrap_or("-"), s.what);
            }
        }
        "summary" => {
            let mut by_what: BTreeMap<String, usize> = BTreeMap::new();
            let mut by_tag: BTreeMap<String, usize> = BTreeMap::new();
            for s in &report.sites {
                let what = if s.what.contains("does not define") {
                    "an async call this code does not define".to_string()
                } else if s.what.contains("stored future") {
                    "`.await` on a stored future".to_string()
                } else if s.what.contains("made here") {
                    "a future made here, awaited elsewhere".to_string()
                } else {
                    s.what.clone()
                };
                *by_what.entry(what).or_default() += 1;
                *by_tag.entry(s.tag.clone().unwrap_or_else(|| "-".into())).or_default() += 1;
            }
            println!("{} waits", report.sites.len());
            for (w, n) in &by_what {
                println!("  {n:5}  {w}");
            }
            println!("by tag:");
            for (t, n) in &by_tag {
                println!("  {n:5}  {t}");
            }
        }
        _ => {}
    }
    for f in &report.findings {
        if mode == "check" {
            println!("{f}");
        }
    }
    if mode == "check" {
        println!("{} finding(s)", report.findings.len());
    }
    // No file read, or no wait found: the directories are wrong, and a green verdict would say so
    // about nothing.
    if mode == "check" && (report.files_scanned == 0 || report.sites.is_empty()) {
        println!("read {} production file(s) and found {} wait(s) — check the --src directories", report.files_scanned, report.sites.len());
        return ExitCode::from(1);
    }
    if report.findings.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

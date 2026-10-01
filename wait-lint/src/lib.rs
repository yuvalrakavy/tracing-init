//! Every place a server waits on something else says why that wait cannot hang it.
//!
//! The rule (Store's no-hang spec, §13.3): **no task that others wait on may wait on a task that
//! can wait on it.** A reviewer can check that for one wait, but not for every wait a codebase
//! will ever add. So each wait carries a one-line tag naming a row of a registry, and the row
//! holds the argument, once:
//!
//! ```text
//! let entry = manager.read().await; // WAIT: manager-lock
//! ```
//!
//! ```text
//! | Key | Kind | Waits on | Held across | Argument |
//! |---|---|---|---|---|
//! | `manager-lock` | acyclic | the store manager's lock | — | only slot changes write it, and they wait on nothing |
//! ```
//!
//! A row is `acyclic` (nothing it waits on can wait back on any of its waiters) or `bounded`
//! (it ends within a stated bound, and the row says what the expiry does).
//!
//! # What is checked
//!
//! * A wait with no tag; a tag naming no row; a row no tag names; a tag that covers no wait.
//! * A timeout tagged with an `acyclic` row: a timeout is a bound, and its row must say what
//!   the expiry does.
//! * **The waiters.** The registry ends with a generated block listing, for each row, every
//!   function holding one of its waits. A new waiter is a diff to that block, in the reviewed
//!   change, so a tag cannot be copied onto a new call path without the row being in front of
//!   its reviewer. `--write` regenerates it.
//! * **Lock order.** A guard (`let g = m.lock()…`) held across another wait is an edge from
//!   the guard's row to the wait's. Each row declares, in its `Held across` column, the waits
//!   its guards may be held across; an undeclared edge is a finding, a declared one no code
//!   takes is a finding, and **a cycle among the declared edges is a finding** — two locks taken
//!   in both orders, or one taken again while held.
//!
//! # What a wait is
//!
//! **Every `.await` is accounted for**, so a wait in a dependency the lint has never heard of
//! is a finding rather than a silence. An `.await` on:
//!
//! * a method named in [`AWAIT_METHODS`] (`lock`, `send`, `recv`, `notified`, `cancelled`, …) or
//!   declared by the registry (`wait-methods`) — **a wait**;
//! * something that is not a call — a stored future: a oneshot reply, a `JoinHandle` — or on
//!   `spawn(..)` / `spawn_blocking(..)` — **a wait**;
//! * `timeout(..)` / `timeout_at(..)` — **a bounded wait**. The async waits inside its arguments
//!   are its own; a blocking call inside them is not, since a timeout cannot interrupt a thread;
//! * a function or method this code defines as `async fn` — **not a wait itself**: it waits
//!   through the waits inside it, which carry their own tags, so a helper is tagged once, where
//!   it actually waits;
//! * a name the registry declares not to wait (`not-waits`: `sleep`, `yield_now`, …) — not a
//!   wait;
//! * an `async { .. }` block — not a wait itself; the waits inside it are visited;
//! * **anything else** — an `async fn` of a dependency — **a wait**, until it is tagged or
//!   declared.
//!
//! An `.await` sees through `.instrument(..)`, `.in_current_span()`, `.boxed()`, `.fuse()` and
//! `Box::pin(..)`.
//!
//! Beside the `.await`s:
//!
//! * `select!`, `join!` and `try_join!`, one wait each; the futures they are given are theirs;
//! * `block_on(..)`, as a function or a method;
//! * blocking calls: a zero-argument `lock()`, `read()`, `write()`, `join()` or `recv()`; any
//!   `wait(..)`, `wait_while(..)` or `blocking_*(..)`; a method the registry declares
//!   (`blocking-methods`); and, as bounded waits, `recv_timeout(..)` and `wait_timeout(..)`;
//! * **a future made in one place and awaited in another**: a `notified()`, `cancelled()`,
//!   `acquire()`, … or a call to this code's own `async fn` that is not awaited, bounded or
//!   spawned where it is made. It is tagged where it is made, since that is where its caller
//!   chose to wait on it.
//!
//! Never a wait: the `try_*` forms, and test code — a `#[cfg(test)]` item, a `#[test]` function,
//! a file a `#[cfg(test)] mod x;` declares, a `tests/` / `benches/` / `examples/` directory, or a
//! `tests.rs` / `*_tests.rs` file.
//!
//! # Where a tag goes
//!
//! `// WAIT: <key>` covers the waits on its own line or on the line directly below its comment
//! block, or — when nothing nearer covers them — the waits of the statement it sits above. A tag
//! covering several waits lists one key per wait, in order: `// WAIT: store-lock, p-reply`. One
//! key over many waits would let the next wait added there pass unread.
//!
//! # What it cannot see
//!
//! What a caller holds across a call into another function: a guard held while calling a helper
//! that waits is not an edge here. A consumer refusing a call that would wait on itself (the
//! runtime guards), the bounded lock waits, and review cover that.

mod registry;
mod scan;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

pub use registry::{Kind, Registry, Row, Waiter};
pub use scan::{AWAIT_METHODS, BLOCKING_ANY_ARG, BLOCKING_ZERO_ARG, BOUNDED_BLOCKING};

/// One wait in production code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// As given to [`check`] (repository-relative).
    pub file: String,
    pub line: usize,
    /// What the wait is, for a message: "`.lock(..).await`", "`select!`", …
    pub what: String,
    /// A timeout: its tag must name a `bounded` row.
    pub bounded: bool,
    /// The enclosing function.
    pub func: String,
    /// The key its tag gives it, if any.
    pub tag: Option<String>,
}

/// A problem the check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub file: String,
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for Finding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "{}:{}: {}", self.file, line, self.message),
            None => write!(f, "{}: {}", self.file, self.message),
        }
    }
}

/// What [`check`] found.
#[derive(Debug, Default)]
pub struct Report {
    pub sites: Vec<Site>,
    pub findings: Vec<Finding>,
    /// The waiters block the registry should hold, rendered.
    pub waiters_block: String,
}

/// Every wait in `files` (path → contents), with the key its tag gives it, and every finding.
/// `files` is the set to lint; test code inside it is recognized and skipped. `registry` is the
/// registry's path (for findings) and its text.
pub fn check(files: &BTreeMap<String, String>, registry: (&str, &str)) -> Report {
    let (registry_file, registry_text) = registry;
    let reg = Registry::parse(registry_text);
    let mut out = Report::default();
    let mut finding = |file: &str, line: Option<usize>, message: String| {
        out.findings.push(Finding { file: file.to_string(), line, message });
    };
    for (line, message) in &reg.problems {
        finding(registry_file, Some(*line), message.clone());
    }
    for row in &reg.rows {
        if row.kind.is_none() {
            finding(registry_file, Some(row.line), format!("row `{}`: its kind must be `acyclic` or `bounded`", row.key));
        }
        for held in &row.held_across {
            if reg.row(held).is_none() {
                finding(registry_file, Some(row.line), format!("row `{}` declares `{held}` held across, and no row has that key", row.key));
            }
        }
    }
    for key in reg.guard_fns.values() {
        if reg.row(key).is_none() {
            finding(registry_file, None, format!("`guard-fns` names `{key}`, and no row has that key"));
        }
    }

    let test_files = declared_test_files(files);
    let mut parsed = Vec::new();
    for (rel, text) in files {
        if is_test_path(rel) || test_files.contains(rel.as_str()) {
            continue;
        }
        match syn::parse_file(text) {
            Ok(f) => parsed.push((rel, text, f)),
            Err(e) => finding(rel, Some(e.span().start().line), format!("`syn` cannot parse this file ({e}), so its waits cannot be read")),
        }
    }
    let mut names = scan::Names::default();
    for (_, _, file) in &parsed {
        names.collect(file);
    }

    // (guard key, wait key, file, line of the wait, line of the guard): every place an order is taken.
    let mut edges: Vec<(String, String, String, usize, usize)> = Vec::new();
    let mut sites = Vec::new();
    for (rel, text, file) in &parsed {
        let scanned = scan::scan(file, &reg, &names);
        let tags = Tags::read(text, &scanned.test_ranges);
        let mut tag_problems = Vec::new();
        let keys = tags.assign(&scanned.found, &mut tag_problems);
        for (line, message) in tag_problems {
            finding(rel, Some(line), message);
        }
        for (found, key) in scanned.found.iter().zip(&keys) {
            sites.push(Site {
                file: (*rel).clone(),
                line: found.line,
                what: found.what.clone(),
                bounded: found.bounded,
                func: found.func.clone(),
                tag: key.clone().flatten(),
            });
            if key.is_none() {
                finding(
                    rel,
                    Some(found.line),
                    format!(
                        "{} is an untagged wait: tag it `// WAIT: <key>`, naming the row of {registry_file} that says why it cannot hang",
                        found.what
                    ),
                );
            }
        }
        for (of, guard_line, wait) in &scanned.edges {
            let guard_key = match of {
                scan::GuardOf::Site(i) => keys[*i].clone().flatten(),
                scan::GuardOf::Key(k) => Some(k.clone()),
            };
            if let (Some(g), Some(Some(Some(w)))) = (guard_key, keys.get(*wait)) {
                edges.push((g, w.clone(), (*rel).clone(), scanned.found[*wait].line, *guard_line));
            }
        }
    }

    let mut used_keys = BTreeSet::new();
    let mut waiters: BTreeSet<Waiter> = BTreeSet::new();
    for site in &sites {
        let Some(key) = &site.tag else { continue };
        match reg.row(key) {
            None => finding(&site.file, Some(site.line), format!("`// WAIT: {key}` names no row of {registry_file}")),
            Some(row) => {
                used_keys.insert(key.clone());
                waiters.insert((key.clone(), site.file.clone(), site.func.clone()));
                if site.bounded && row.kind == Some(Kind::Acyclic) {
                    finding(
                        &site.file,
                        Some(site.line),
                        format!("{} names `{key}`, an `acyclic` row: a timeout is a bound, and its row must say what the expiry does", site.what),
                    );
                }
            }
        }
    }
    for row in &reg.rows {
        if !used_keys.contains(&row.key) {
            finding(registry_file, Some(row.line), format!("row `{}` is named by no wait — delete it, or tag the waits it covers", row.key));
        }
    }

    // The waiters block.
    match &reg.waiters {
        None => finding(registry_file, None, "has no `wait-lint-waiters` block: generate it with `wait-lint --write`".into()),
        Some(listed) => {
            for w in waiters.iter().filter(|w| !listed.contains_key(*w)) {
                finding(
                    registry_file,
                    None,
                    format!(
                        "`{}` now has a wait in `{}` ({}): re-read row `{}` against this caller, then `wait-lint --write`",
                        w.0, w.2, w.1, w.0
                    ),
                );
            }
            for (w, line) in listed.iter().filter(|(w, _)| !waiters.contains(*w)) {
                finding(registry_file, Some(*line), format!("`{} {} {}` holds no such wait any more: `wait-lint --write`", w.0, w.1, w.2));
            }
        }
    }
    out.waiters_block = registry::render_waiters(&waiters);

    // Lock order.
    let mut taken: BTreeSet<(String, String)> = BTreeSet::new();
    for (g, w, file, wait_line, guard_line) in &edges {
        let Some(row) = reg.row(g) else { continue };
        if reg.row(w).is_none() {
            continue;
        }
        taken.insert((g.clone(), w.clone()));
        if !row.held_across.contains(w) {
            finding(
                file,
                Some(*wait_line),
                format!(
                    "a `{g}` guard (line {guard_line}) is held across this `{w}` wait: declare `{w}` in row `{g}`'s Held across, with why that order cannot close a cycle, or release the guard first"
                ),
            );
        }
    }
    for row in &reg.rows {
        for held in &row.held_across {
            if reg.row(held).is_some() && !taken.contains(&(row.key.clone(), held.clone())) {
                finding(registry_file, Some(row.line), format!("row `{}` declares `{held}` held across, and no `{}` guard is held across one", row.key, row.key));
            }
        }
    }
    for cycle in cycles(&reg) {
        let first = reg.row(&cycle[0]).map(|r| r.line);
        finding(registry_file, first, format!("the declared lock order has a cycle: {}", cycle.join(" → ")));
    }

    out.sites = sites;
    out.findings.sort_by(|a, b| (&a.file, a.line, &a.message).cmp(&(&b.file, b.line, &b.message)));
    out
}

/// Each elementary cycle of the declared `Held across` edges, once, starting at its least key.
fn cycles(reg: &Registry) -> Vec<Vec<String>> {
    let graph: BTreeMap<&str, Vec<&str>> = reg
        .rows
        .iter()
        .map(|r| (r.key.as_str(), r.held_across.iter().map(String::as_str).filter(|k| reg.row(k).is_some()).collect()))
        .collect();
    let mut found = Vec::new();
    for &start in graph.keys() {
        // Paths from `start` through keys greater than it, back to `start`.
        let mut stack = vec![(start, vec![start])];
        while let Some((at, path)) = stack.pop() {
            for &next in graph.get(at).map(Vec::as_slice).unwrap_or(&[]) {
                if next == start {
                    let mut cycle: Vec<String> = path.iter().map(|s| s.to_string()).collect();
                    cycle.push(start.to_string());
                    found.push(cycle);
                } else if next > start && !path.contains(&next) {
                    let mut p = path.clone();
                    p.push(next);
                    stack.push((next, p));
                }
            }
        }
    }
    found
}

/// Read every `.rs` under `root/<dir>` for each of `src_dirs`, and the registry at
/// `root/<registry>`, then [`check`] them. Paths in the result are relative to `root`.
pub fn check_dirs(root: &Path, src_dirs: &[&str], registry: &str) -> std::io::Result<Report> {
    let mut files = BTreeMap::new();
    for dir in src_dirs {
        collect_rs(root, &root.join(dir), &mut files)?;
    }
    let registry_text = std::fs::read_to_string(root.join(registry))?;
    Ok(check(&files, (registry, &registry_text)))
}

/// Rewrite the registry's waiters block from the tree. Returns whether it changed.
pub fn write_waiters(root: &Path, src_dirs: &[&str], registry: &str) -> std::io::Result<bool> {
    let report = check_dirs(root, src_dirs, registry)?;
    let path = root.join(registry);
    let text = std::fs::read_to_string(&path)?;
    let updated = registry::with_waiters(&text, &report.waiters_block);
    if updated == text {
        return Ok(false);
    }
    std::fs::write(&path, updated)?;
    Ok(true)
}

/// For a crate's own test: panic, listing every finding, unless the crate's waits are all
/// registered.
///
/// ```no_run
/// #[test]
/// fn every_wait_is_registered() {
///     wait_lint::assert_registered(env!("CARGO_MANIFEST_DIR"), &["src"], "docs/wait-registry.md");
/// }
/// ```
pub fn assert_registered(root: impl AsRef<Path>, src_dirs: &[&str], registry: &str) {
    let report = check_dirs(root.as_ref(), src_dirs, registry).expect("read the sources and the registry");
    assert!(
        report.findings.is_empty(),
        "{} wait-registry finding(s):\n{}",
        report.findings.len(),
        report.findings.iter().map(|f| format!("  {f}")).collect::<Vec<_>>().join("\n")
    );
}

fn collect_rs(root: &Path, dir: &Path, out: &mut BTreeMap<String, String>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(root, &path, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().replace('\\', "/");
            out.insert(rel, std::fs::read_to_string(&path)?);
        }
    }
    Ok(())
}

/// A path that is test code by where it lives or what it is called.
pub fn is_test_path(rel: &str) -> bool {
    let mut parts: Vec<&str> = rel.split('/').collect();
    let name = parts.pop().unwrap_or("");
    parts.iter().any(|d| matches!(*d, "tests" | "benches" | "examples")) || name == "tests.rs" || name.ends_with("_tests.rs")
}

/// Files a `#[cfg(test)] mod name;` declaration pulls in: `dir/name.rs` or `dir/name/mod.rs`.
fn declared_test_files(files: &BTreeMap<String, String>) -> BTreeSet<&str> {
    let mut out = BTreeSet::new();
    for (rel, text) in files {
        let dir = module_dir(rel);
        let mut cfg_test = false;
        let mut has_path = false;
        for line in text.lines() {
            let t = line.trim();
            if t.starts_with("#[cfg(test)]") {
                cfg_test = true;
                continue;
            }
            if t.starts_with("#[path") {
                has_path = true;
                continue;
            }
            if cfg_test && !has_path {
                if let Some(name) = external_mod_name(t) {
                    for candidate in [format!("{dir}{name}.rs"), format!("{dir}{name}/mod.rs")] {
                        if let Some((key, _)) = files.get_key_value(&candidate) {
                            out.insert(key.as_str());
                        }
                    }
                }
            }
            if !t.starts_with("#[") && !t.is_empty() {
                cfg_test = false;
                has_path = false;
            }
        }
    }
    out
}

fn external_mod_name(line: &str) -> Option<&str> {
    let rest = line.strip_suffix(';')?;
    let idx = rest.find("mod ")?;
    let (vis, name) = (&rest[..idx], rest[idx + 4..].trim());
    let vis_ok = vis.trim().is_empty() || vis.trim().starts_with("pub");
    (vis_ok && !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')).then_some(name)
}

fn module_dir(rel: &str) -> String {
    let (dir, name) = rel.rsplit_once('/').unwrap_or(("", rel));
    let prefix = if dir.is_empty() { String::new() } else { format!("{dir}/") };
    match name {
        "lib.rs" | "main.rs" | "mod.rs" => prefix,
        other => format!("{prefix}{}/", other.trim_end_matches(".rs")),
    }
}

/// The `// WAIT: k1, k2` tags of one file, by line.
struct Tags<'a> {
    lines: Vec<&'a str>,
    by_line: BTreeMap<usize, Vec<String>>,
    malformed: Vec<usize>,
}

impl<'a> Tags<'a> {
    fn read(text: &'a str, test_ranges: &[(usize, usize)]) -> Tags<'a> {
        let lines: Vec<&str> = text.lines().collect();
        let mut by_line = BTreeMap::new();
        let mut malformed = Vec::new();
        for (i, line) in lines.iter().enumerate() {
            let n = i + 1;
            if test_ranges.iter().any(|(a, b)| (*a..=*b).contains(&n)) {
                continue;
            }
            match tag_on(line) {
                Some(Ok(keys)) => {
                    by_line.insert(n, keys);
                }
                Some(Err(())) => malformed.push(n),
                None => {}
            }
        }
        Tags { lines, by_line, malformed }
    }

    /// The nearest tag line for a wait on `line` in the statement starting at `stmt_first`.
    fn nearest(&self, line: usize, stmt_first: Option<usize>) -> Option<usize> {
        let mut anchors = vec![line];
        if let Some(first) = stmt_first.filter(|f| *f != line) {
            anchors.push(first);
        }
        for anchor in anchors {
            if self.by_line.contains_key(&anchor) {
                return Some(anchor);
            }
            let mut n = anchor;
            while n > 1 {
                n -= 1;
                let t = self.lines.get(n - 1).map(|l| l.trim()).unwrap_or("");
                if !t.starts_with("//") {
                    break;
                }
                if self.by_line.contains_key(&n) {
                    return Some(n);
                }
            }
        }
        None
    }

    /// The key each wait gets: `None` untagged, `Some(None)` covered by a tag that is wrong
    /// (reported in `problems`), `Some(Some(key))` keyed.
    fn assign(&self, found: &[scan::Found], problems: &mut Vec<(usize, String)>) -> Vec<Option<Option<String>>> {
        let mut covered: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
        for (i, f) in found.iter().enumerate() {
            if let Some(tag_line) = self.nearest(f.line, f.stmt.map(|s| s.0)) {
                covered.entry(tag_line).or_default().push(i);
            }
        }
        let mut keys = vec![None; found.len()];
        for (tag_line, mut waits) in covered {
            let tag = &self.by_line[&tag_line];
            waits.sort_by_key(|&i| (found[i].line, found[i].column));
            if tag.len() == waits.len() {
                for (i, key) in waits.iter().zip(tag) {
                    keys[*i] = Some(Some(key.clone()));
                }
            } else {
                let whats: Vec<String> = waits.iter().map(|&i| format!("{} (line {})", found[i].what, found[i].line)).collect();
                problems.push((
                    tag_line,
                    format!(
                        "this tag names {} key(s) for {} wait(s): list one key per wait, in order — {}",
                        tag.len(),
                        waits.len(),
                        whats.join(", ")
                    ),
                ));
                for i in waits {
                    keys[i] = Some(None);
                }
            }
        }
        let used: BTreeSet<usize> = found
            .iter()
            .enumerate()
            .filter_map(|(i, f)| keys[i].as_ref().and(self.nearest(f.line, f.stmt.map(|s| s.0))))
            .collect();
        for (line, tag) in &self.by_line {
            if !used.contains(line) {
                problems.push((*line, format!("`// WAIT: {}` covers no wait — remove it, or move it onto the wait it means", tag.join(", "))));
            }
        }
        for line in &self.malformed {
            problems.push((*line, "a malformed `// WAIT:` tag: keys are lowercase letters, digits and `-`, separated by commas".into()));
        }
        keys
    }
}

/// `// WAIT: k1, k2` anywhere on a line → the keys; `Err` when the tag is there but malformed.
fn tag_on(line: &str) -> Option<Result<Vec<String>, ()>> {
    let idx = line.find("//")?;
    let rest = line[idx + 2..].trim_start_matches('/').trim_start();
    let rest = rest.strip_prefix("WAIT:")?;
    // The keys run to the end of the line, or to prose after them (`// WAIT: k — why`).
    let list = rest.split(|c: char| c == '—' || c == '(' || c == ';').next().unwrap_or("");
    let keys: Vec<String> = list.split(',').map(|k| k.trim().to_string()).collect();
    if keys.iter().all(|k| registry::is_key(k)) {
        Some(Ok(keys))
    } else {
        Some(Err(()))
    }
}

#[cfg(test)]
mod tests;

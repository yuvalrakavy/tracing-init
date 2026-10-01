//! The registry document: a markdown table with one row per thing waited on, a settings block,
//! and the generated list of the functions each row's waits sit in.

use std::collections::{BTreeMap, BTreeSet};

/// The info string of the settings block.
pub const SETTINGS_BLOCK: &str = "wait-lint";
/// The info string of the generated waiters block.
pub const WAITERS_BLOCK: &str = "wait-lint-waiters";

/// A row's kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Nothing it waits on can wait back on any of its waiters.
    Acyclic,
    /// It ends within a stated bound, and the row says what the expiry does.
    Bounded,
}

/// One row: one thing waited on.
#[derive(Debug, Clone)]
pub struct Row {
    pub key: String,
    /// `None` when the kind is neither `acyclic` nor `bounded` (a finding).
    pub kind: Option<Kind>,
    /// The waits allowed while a guard of this row is held: the lock-order edges it declares.
    pub held_across: BTreeSet<String>,
    /// 1-based line in the registry.
    pub line: usize,
}

#[derive(Debug, Clone, Copy)]
struct Columns {
    held: Option<usize>,
    waits_on: Option<usize>,
    argument: Option<usize>,
}

/// `(key, file, function)`: a function that holds a wait of that row.
pub type Waiter = (String, String, String);

/// The parsed registry.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    pub rows: Vec<Row>,
    /// Extra methods that wait when awaited (rumqttc's `publish`, …).
    pub wait_methods: BTreeSet<String>,
    /// Extra methods that block when called (a `sync_channel`'s `send`, …).
    pub blocking_methods: BTreeSet<String>,
    /// Functions and methods whose `.await` waits on nothing in this process (`sleep`, …).
    pub not_waits: BTreeSet<String>,
    /// Functions that return a guard of a row (`write_with_bound: store-lock`): a caller binding
    /// their result holds that row's guard.
    pub guard_fns: BTreeMap<String, String>,
    /// The generated waiters block, if present: each waiter and its line.
    pub waiters: Option<BTreeMap<Waiter, usize>>,
    /// Format problems: `(line, message)`.
    pub problems: Vec<(usize, String)>,
}

pub fn is_key(s: &str) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Split a comma-separated list of keys, tolerating backticks.
pub fn key_list(cell: &str) -> Vec<String> {
    cell.split(',')
        .map(|k| k.trim().trim_matches('`').trim().to_string())
        .filter(|k| !k.is_empty() && k != "-" && k != "—")
        .collect()
}

impl Registry {
    pub fn parse(text: &str) -> Registry {
        let mut reg = Registry::default();
        // Column indices of the table being read.
        let mut table: Option<Columns> = None;
        let mut block: Option<&str> = None;
        let mut seen = BTreeSet::new();
        for (i, raw) in text.lines().enumerate() {
            let line_no = i + 1;
            let t = raw.trim();
            if let Some(kind) = block {
                if t.starts_with("```") {
                    block = None;
                    continue;
                }
                if t.is_empty() || t.starts_with('#') {
                    continue;
                }
                if kind == WAITERS_BLOCK {
                    let parts: Vec<&str> = t.split_whitespace().collect();
                    if parts.len() == 3 {
                        let waiter = (parts[0].to_string(), parts[1].to_string(), parts[2].to_string());
                        reg.waiters.get_or_insert_with(BTreeMap::new).insert(waiter, line_no);
                    } else {
                        reg.problems.push((line_no, format!("not a `key file function` waiter line: `{t}`")));
                    }
                    continue;
                }
                match t.split_once('=') {
                    Some((name, values)) => {
                        let values = values.split(',').map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
                        match name.trim() {
                            "wait-methods" => reg.wait_methods.extend(values),
                            "blocking-methods" => reg.blocking_methods.extend(values),
                            "not-waits" => reg.not_waits.extend(values),
                            "guard-fns" => {
                                for pair in values {
                                    match pair.split_once(':') {
                                        Some((f, k)) if is_key(k.trim()) => {
                                            reg.guard_fns.insert(f.trim().to_string(), k.trim().to_string());
                                        }
                                        _ => reg.problems.push((line_no, format!("not a `function: key` pair: `{pair}`"))),
                                    }
                                }
                            }
                            other => reg.problems.push((line_no, format!("unknown setting `{other}`"))),
                        }
                    }
                    None => reg.problems.push((line_no, format!("not a `name = values` setting: `{t}`"))),
                }
                continue;
            }
            if let Some(info) = t.strip_prefix("```") {
                match info.trim() {
                    SETTINGS_BLOCK => block = Some(SETTINGS_BLOCK),
                    WAITERS_BLOCK => {
                        block = Some(WAITERS_BLOCK);
                        reg.waiters.get_or_insert_with(BTreeMap::new);
                    }
                    _ => {}
                }
                continue;
            }
            if !t.starts_with('|') {
                table = None;
                continue;
            }
            let cells: Vec<&str> = t.trim_matches('|').split('|').map(str::trim).collect();
            let Some(cols) = table else {
                let lower: Vec<String> = cells.iter().map(|c| c.to_ascii_lowercase()).collect();
                if lower.first().map(String::as_str) == Some("key") && lower.get(1).map(String::as_str) == Some("kind") {
                    let at = |name: &str| lower.iter().position(|c| c == name);
                    table = Some(Columns { held: at("held across"), waits_on: at("waits on"), argument: at("argument") });
                    if at("waits on").is_none() || at("argument").is_none() {
                        reg.problems.push((line_no, "the table needs `Waits on` and `Argument` columns".into()));
                    }
                }
                continue;
            };
            let (key_at, kind_at) = (0, 1);
            if cells.iter().all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':')) {
                continue;
            }
            let key = cells.get(key_at).map(|k| k.trim_matches('`')).unwrap_or("").to_string();
            let kind = match cells.get(kind_at).map(|k| k.trim_matches(|c| c == '*' || c == '`').to_ascii_lowercase()) {
                Some(k) if k == "acyclic" => Some(Kind::Acyclic),
                Some(k) if k == "bounded" => Some(Kind::Bounded),
                _ => None,
            };
            if !is_key(&key) {
                reg.problems.push((line_no, format!("`{key}` is not a key: lowercase letters, digits and `-`")));
                continue;
            }
            if !seen.insert(key.clone()) {
                reg.problems.push((line_no, format!("`{key}` is a second row with the same key")));
                continue;
            }
            let held_across = cols.held.and_then(|at| cells.get(at)).map(|c| key_list(c)).unwrap_or_default();
            let cell = |at: Option<usize>| at.and_then(|a| cells.get(a)).map(|c| c.trim()).unwrap_or("");
            if cell(cols.waits_on).is_empty() {
                reg.problems.push((line_no, format!("row `{key}` does not say what it waits on")));
            }
            let argument = cell(cols.argument);
            if argument.is_empty() {
                reg.problems.push((line_no, format!("row `{key}` has no argument")));
            } else if kind == Some(Kind::Bounded) && !argument.to_ascii_lowercase().contains("on expiry") {
                reg.problems.push((line_no, format!("row `{key}` is `bounded`: its argument must say what happens `on expiry`")));
            }
            reg.rows.push(Row { key, kind, held_across: held_across.into_iter().collect(), line: line_no });
        }
        if block.is_some() {
            reg.problems.push((text.lines().count(), "a fenced block is never closed".into()));
        }
        reg
    }

    pub fn row(&self, key: &str) -> Option<&Row> {
        self.rows.iter().find(|r| r.key == key)
    }
}

/// The waiters block for `waiters`.
pub fn render_waiters(waiters: &BTreeSet<Waiter>) -> String {
    let mut out = format!("```{WAITERS_BLOCK}\n");
    for (key, file, func) in waiters {
        out.push_str(&format!("{key} {file} {func}\n"));
    }
    out.push_str("```\n");
    out
}

/// `text` with its waiters block replaced by `block` (appended when it has none).
pub fn with_waiters(text: &str, block: &str) -> String {
    let start_marker = format!("```{WAITERS_BLOCK}");
    let lines: Vec<&str> = text.lines().collect();
    if let Some(start) = lines.iter().position(|l| l.trim() == start_marker) {
        if let Some(len) = lines[start + 1..].iter().position(|l| l.trim().starts_with("```")) {
            let mut out: Vec<String> = lines[..start].iter().map(|l| l.to_string()).collect();
            out.push(block.trim_end().to_string());
            out.extend(lines[start + 2 + len..].iter().map(|l| l.to_string()));
            return out.join("\n") + "\n";
        }
    }
    let mut out = text.trim_end().to_string();
    out.push_str("\n\n");
    out.push_str(block);
    out
}

//! Finding the waits in one parsed file: what each is, and the statement and function it sits
//! in; and the raw lock types it names.

use std::collections::{BTreeMap, BTreeSet};

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::registry::Registry;

/// Methods that wait on another task when awaited.
pub const AWAIT_METHODS: &[&str] = &[
    "lock",
    "read",
    "write",
    "lock_owned",
    "read_owned",
    "write_owned",
    "send",
    "reserve",
    "reserve_owned",
    "recv",
    "recv_many",
    "notified",
    "acquire",
    "acquire_owned",
    "acquire_many",
    "acquire_many_owned",
    "changed",
    "wait_for",
    "wait",
    "join_next",
    "cancelled",
    // A pinned stored future, polled by reference.
    "as_mut",
];

/// Blocking methods when called with no arguments: `Mutex::lock`, `RwLock::read`/`write`,
/// `JoinHandle::join`, `Receiver::recv`. With arguments these names are mostly something else
/// (`Path::join`, a storage `read(key)`).
pub const BLOCKING_ZERO_ARG: &[&str] = &["lock", "read", "write", "join", "recv"];

/// Blocking methods with any arguments.
pub const BLOCKING_ANY_ARG: &[&str] = &[
    "wait",
    "wait_while",
    "block_on",
    "blocking_send",
    "blocking_recv",
    "blocking_lock",
    "blocking_read",
    "blocking_write",
    "blocking_lock_owned",
];

/// Blocking methods bounded by their argument: a bounded wait, still a wait.
pub const BOUNDED_BLOCKING: &[&str] = &["recv_timeout", "wait_timeout", "wait_timeout_while"];

/// Methods that make a future unambiguously: one made and not awaited where it is made is a
/// wait that escapes.
const ESCAPING_METHODS: &[&str] = &[
    "notified",
    "cancelled",
    "changed",
    "acquire",
    "acquire_owned",
    "acquire_many",
    "acquire_many_owned",
    "reserve",
    "reserve_owned",
    "join_next",
    "recv_many",
    "wait_for",
    "lock_owned",
    "read_owned",
    "write_owned",
];

/// Functions (and methods) whose `.await` waits on a task they start, and whose future argument
/// runs as that task — beyond the reach of any timeout around the call.
const SPAWNING_FNS: &[&str] = &["spawn", "spawn_blocking", "spawn_local"];

/// Functions whose last argument is a future they bound.
const BOUNDING_FNS: &[&str] = &["timeout", "timeout_at"];

/// Macros that are each one wait on the futures they are given.
pub const WAITING_MACROS: &[&str] = &["select", "select_biased", "join", "try_join"];

/// Wrappers an `.await` sees through: `f().instrument(span).await` awaits `f()`.
const TRANSPARENT_METHODS: &[&str] = &["instrument", "in_current_span", "boxed", "fuse", "catch_unwind"];
/// Methods whose last argument is the future really awaited: a task-local's `KEY.scope(v, fut)`.
const TRANSPARENT_LAST_ARG_METHODS: &[&str] = &["scope", "sync_scope"];
const TRANSPARENT_FNS: &[&str] = &["pin", "branch", "AssertUnwindSafe"];
/// lock_order's consumer wrappers: their last argument is the future really awaited.
const TRANSPARENT_LAST_ARG_FNS: &[&str] = &["holding", "holding_in"];

/// Names too common to read a method call into one of this code's `async fn`s: `x.get(k)` is
/// not a future of a local `async fn get`.
const COMMON_NAMES: &[&str] = &[
    "new", "get", "set", "insert", "remove", "push", "pop", "clear", "len", "is_empty", "send", "recv", "read", "write", "lock", "run",
    "start", "stop", "close", "flush", "next", "call", "execute", "handle", "update", "load", "save", "init", "connect", "open", "wait",
    "apply", "build", "from", "into", "clone", "drop", "drain", "take", "poll", "tick", "flush",
];

/// What this code defines, from its production (non-test) items only: a test helper named like
/// a dependency's function must not excuse that function.
#[derive(Debug, Default)]
pub struct Names {
    pub local_async: BTreeSet<String>,
    pub local_sync: BTreeSet<String>,
    /// Module and type names: a path starting with one is a path into this code.
    pub local_paths: BTreeSet<String>,
    /// What every file's `use`s bind, read through that file's own (see [`Uses`]): a path into
    /// this crate (`crate::sync::Mutex`, `super::sync::Mutex`) can name what any module bound — a
    /// private `use std::sync` at the root is `crate::sync` to every module.
    pub bound: BTreeMap<String, Vec<Vec<String>>>,
    /// The crate's own names as other crates of its package write them (`mqtt_ynca::…` in its
    /// `src/bin/`), from its `Cargo.toml` when the scan has one: a path starting with one is a path
    /// into this code.
    pub own_crates: BTreeSet<String>,
}

impl Names {
    pub fn collect(&mut self, file: &syn::File) {
        let uses = Uses::of(file);
        for (name, paths) in &uses.bound {
            for path in paths {
                for reading in uses.readings(path).unwrap_or_else(|| vec![path.clone()]) {
                    let entry = self.bound.entry(name.clone()).or_default();
                    if !entry.contains(&reading) {
                        entry.push(reading);
                    }
                }
            }
        }
        struct C<'a>(&'a mut Names);
        impl<'ast> Visit<'ast> for C<'_> {
            fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
                if !is_test_item(&f.attrs) {
                    syn::visit::visit_item_fn(self, f);
                }
            }
            fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
                if !is_test_item(&f.attrs) {
                    syn::visit::visit_impl_item_fn(self, f);
                }
            }
            fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
                if !is_test_item(&f.attrs) {
                    syn::visit::visit_trait_item_fn(self, f);
                }
            }
            fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
                if !is_test_item(&m.attrs) {
                    self.0.local_paths.insert(m.ident.to_string());
                    syn::visit::visit_item_mod(self, m);
                }
            }
            fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
                if !is_test_item(&i.attrs) {
                    if let Some(t) = type_name(&i.self_ty) {
                        self.0.local_paths.insert(t);
                    }
                    // Its trait impls are this crate's own async fns too; another crate's calls
                    // to the same names are not excused, since names are per crate.
                    syn::visit::visit_item_impl(self, i);
                }
            }
            fn visit_item_struct(&mut self, s: &'ast syn::ItemStruct) {
                self.0.local_paths.insert(s.ident.to_string());
            }
            fn visit_item_enum(&mut self, e: &'ast syn::ItemEnum) {
                self.0.local_paths.insert(e.ident.to_string());
            }
            fn visit_item_trait(&mut self, t: &'ast syn::ItemTrait) {
                if !is_test_item(&t.attrs) {
                    self.0.local_paths.insert(t.ident.to_string());
                    syn::visit::visit_item_trait(self, t);
                }
            }
            fn visit_signature(&mut self, sig: &'ast syn::Signature) {
                let name = sig.ident.to_string();
                if sig.asyncness.is_some() {
                    self.0.local_async.insert(name);
                } else {
                    self.0.local_sync.insert(name);
                }
                syn::visit::visit_signature(self, sig);
            }
        }
        C(self).visit_file(file);
    }

    /// A call through `path` reaches one of this code's `async fn`s: a bare name, or a path that
    /// starts in this code (`crate::`, `self::`, `Self::`, a local module or type).
    fn is_local_fn(&self, path: &[String]) -> bool {
        let Some(last) = path.last() else { return false };
        if !self.local_async.contains(last) {
            return false;
        }
        match path.first().map(String::as_str) {
            _ if path.len() == 1 => true,
            Some("crate" | "self" | "super" | "Self") => true,
            Some(first) => self.local_paths.contains(first) || self.own_crates.contains(first),
            None => false,
        }
    }

    /// A method call reaches one of this code's `async fn`s. Name-based, so common names never
    /// count: the method could be anyone's.
    fn is_local_method(&self, name: &str) -> bool {
        self.local_async.contains(name) && !COMMON_NAMES.contains(&name)
    }

    fn escapes_fn(&self, path: &[String]) -> bool {
        self.is_local_fn(path) && path.last().is_some_and(|n| !self.local_sync.contains(n) && !COMMON_NAMES.contains(&n.as_str()))
    }

    fn escapes_method(&self, name: &str) -> bool {
        self.is_local_method(name) && !self.local_sync.contains(name)
    }

    /// A path that starts in this code: `crate`/`self`/`super`, or one of its modules or types.
    fn is_local_path(&self, path: &[String]) -> bool {
        path.first().is_some_and(|f| {
            matches!(f.as_str(), "crate" | "self" | "super" | "Self") || self.local_paths.contains(f) || self.own_crates.contains(f)
        })
    }
}

/// What one file's `use` declarations and `extern crate .. as ..` items bind — a name → every path
/// it stands for — and what it glob-imports, from production items only. Read for the whole file,
/// not per module: a name bound two ways in one file has both readings.
#[derive(Debug, Default)]
pub struct Uses {
    bound: BTreeMap<String, Vec<Vec<String>>>,
    globs: Vec<Vec<String>>,
    /// The free functions the file defines: a bare call to one is the file's own.
    fns: BTreeSet<String>,
}

impl Uses {
    pub fn of(file: &syn::File) -> Uses {
        struct C(Uses);
        impl<'ast> Visit<'ast> for C {
            fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
                if !is_test_item(&u.attrs) {
                    self.0.read_tree(&u.tree, &mut Vec::new());
                }
            }
            fn visit_item_extern_crate(&mut self, e: &'ast syn::ItemExternCrate) {
                if let (false, Some((_, rename))) = (is_test_item(&e.attrs), &e.rename) {
                    self.0.bind(rename.to_string(), vec![e.ident.to_string()]);
                }
            }
            fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
                if !is_test_item(&f.attrs) {
                    self.0.fns.insert(f.sig.ident.to_string());
                    syn::visit::visit_item_fn(self, f);
                }
            }
            fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
                if !is_test_item(&f.attrs) {
                    syn::visit::visit_impl_item_fn(self, f);
                }
            }
            fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
                if !is_test_item(&m.attrs) {
                    syn::visit::visit_item_mod(self, m);
                }
            }
            fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
                if !is_test_item(&i.attrs) {
                    syn::visit::visit_item_impl(self, i);
                }
            }
        }
        let mut c = C(Uses::default());
        c.visit_file(file);
        c.0
    }

    fn read_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.read_tree(&p.tree, prefix);
                prefix.pop();
            }
            // `use std::sync::{self}` binds `sync`; `use std::sync::{self as s}` binds `s`.
            syn::UseTree::Name(n) if n.ident == "self" => {
                if let Some(last) = prefix.last().cloned() {
                    self.bind(last, prefix.clone());
                }
            }
            syn::UseTree::Name(n) => {
                let mut path = prefix.clone();
                path.push(n.ident.to_string());
                self.bind(n.ident.to_string(), path);
            }
            syn::UseTree::Rename(r) if r.rename == "_" => {}
            syn::UseTree::Rename(r) => {
                let mut path = prefix.clone();
                if r.ident != "self" {
                    path.push(r.ident.to_string());
                }
                self.bind(r.rename.to_string(), path);
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.read_tree(t, prefix);
                }
            }
            syn::UseTree::Glob(_) => self.globs.push(prefix.clone()),
        }
    }

    fn bind(&mut self, name: String, path: Vec<String>) {
        if path.len() == 1 && path[0] == name {
            return; // `use std;`, `extern crate tokio;`: the name is already itself
        }
        let entry = self.bound.entry(name).or_default();
        if !entry.contains(&path) {
            entry.push(path);
        }
    }

    /// Every reading of `segs` through this file's bindings, chained (`use std::sync; use sync as
    /// s;`) a few deep; `None` when no binding names its first segment.
    pub fn readings(&self, segs: &[String]) -> Option<Vec<Vec<String>>> {
        self.bound.get(segs.first()?)?;
        let mut done = Vec::new();
        let mut todo = vec![segs.to_vec()];
        for _ in 0..4 {
            let mut next = Vec::new();
            for path in todo {
                match path.first().and_then(|f| self.bound.get(f)) {
                    Some(targets) => {
                        for target in targets {
                            let mut reading = target.clone();
                            reading.extend_from_slice(&path[1..]);
                            next.push(reading);
                        }
                    }
                    None => done.push(path),
                }
            }
            todo = next;
            if todo.is_empty() {
                break;
            }
        }
        done.extend(todo); // bound deeper than that: taken as it stands
        Some(done)
    }

    /// The file glob-imports from outside this code (`use dep::*`), so a bare name it neither
    /// defines nor imports by name may be a dependency's.
    fn globs_outside(&self, names: &Names) -> bool {
        self.globs.iter().any(|g| self.readings(g).unwrap_or_else(|| vec![g.clone()]).iter().any(|r| !names.is_local_path(r)))
    }
}

/// Whose function a call reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Callee {
    /// One of this code's `async fn`s.
    Local,
    /// Not this code's: a dependency's, or a name this code does not define as async.
    Foreign,
    /// One of this code's async names, called bare in a file that glob-imports from a dependency
    /// that may define it too.
    Ambiguous,
}

/// A wait found in the syntax.
#[derive(Debug, Clone)]
pub struct Found {
    pub line: usize,
    pub column: usize,
    /// `(first line, last line)` of the innermost statement holding it.
    pub stmt: Option<(usize, usize)>,
    pub what: String,
    /// A timeout: its tag must name a `bounded` row.
    pub bounded: bool,
    /// The enclosing function, with its inline modules and owner: `m::Type::method`.
    pub func: String,
}

/// `((line, column), (line, column))`, start and end, of a string literal.
pub type Span2 = ((usize, usize), (usize, usize));

/// One file's waits.
#[derive(Debug, Default)]
pub struct FileScan {
    pub found: Vec<Found>,
    /// Raw lock types named in production code: `(line, path)` (the `raw-locks` rule).
    pub raw_locks: Vec<(usize, String)>,
    /// `.name(..).await` on a receiver other than `self`, where `name` is one of this code's async
    /// methods and the registry says neither `wait-methods` nor `local-methods`: `(line, name)`.
    /// The lint matches names, not types, so such a call may be a dependency's wait hiding behind
    /// this code's name (Store no-hang §14.1: mqtt_hdl's own `publish` hid rumqttc's).
    pub ambiguous: Vec<(usize, String)>,
    /// `name(..).await`, bare, where `name` is one of this code's async fns that this file neither
    /// defines nor imports by name, and the file glob-imports from a dependency, which may define
    /// it too: `(line, name)`. Resolved by the same declarations as [`Self::ambiguous`].
    pub ambiguous_fns: Vec<(usize, String)>,
    /// lock_order classes named by a string literal: `(line, class)` — each must be a registry key.
    pub classes: Vec<(usize, String)>,
    /// Line ranges of test items, whose tags are not production tags.
    pub test_ranges: Vec<(usize, usize)>,
    /// String literals: a `// WAIT:` inside one is text, not a tag.
    pub literals: Vec<Span2>,
}

pub fn scan(file: &syn::File, reg: &Registry, names: &Names) -> FileScan {
    let mut v = Visitor {
        reg,
        names,
        uses: Uses::of(file),
        test_depth: 0,
        bounded_depth: 0,
        bounding_future: 0,
        token_depth: 0,
        stmts: Vec::new(),
        consumed: BTreeSet::new(),
        funcs: Vec::new(),
        owners: Vec::new(),
        mods: Vec::new(),
        out: FileScan::default(),
    };
    v.visit_file(file);
    v.out
}

struct Visitor<'r> {
    reg: &'r Registry,
    names: &'r Names,
    /// What this file's `use`s bind: a path is read through them.
    uses: Uses,
    test_depth: usize,
    /// Inside a future a timeout bounds: its async waits are the timeout's.
    bounded_depth: usize,
    /// Visiting a timeout's future argument: an `async` block here runs under the timeout.
    bounding_future: usize,
    /// Inside a macro's tokens.
    token_depth: usize,
    stmts: Vec<(usize, usize)>,
    /// `(line, column)` of calls whose future is awaited, bounded, spawned or dropped where it
    /// is made.
    consumed: BTreeSet<(usize, usize)>,
    funcs: Vec<String>,
    /// The `impl` or `trait` being visited, for `Type::method` / `<Type as Trait>::method`.
    owners: Vec<String>,
    /// Inline modules being visited.
    mods: Vec<String>,
    out: FileScan,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Wait {
    /// An async wait: a timeout around it bounds it.
    Async,
    /// A blocking call: a timeout cannot interrupt a thread.
    Blocking,
}

/// What a token stream being scanned is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scan {
    Code,
    /// A `select!` body: branch futures between `=` and `=>`, handlers after.
    Select,
    /// Arguments that are futures (a `join!`'s).
    Futures,
    /// A timeout's arguments: futures, and its `async` blocks run under it.
    TimeoutArgs,
}

impl Scan {
    fn for_macro(name: &str) -> Scan {
        match name {
            "select" | "select_biased" => Scan::Select,
            "join" | "try_join" => Scan::Futures,
            _ => Scan::Code,
        }
    }
}

/// Split a cfg list on its top-level commas.
fn cfg_items(ts: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut items = vec![Vec::new()];
    for tt in ts {
        match &tt {
            TokenTree::Punct(p) if p.as_char() == ',' => items.push(Vec::new()),
            _ => items.last_mut().expect("never empty").push(tt),
        }
    }
    items.retain(|i| !i.is_empty());
    items
}

/// Does this cfg predicate hold only in test builds? `test`, `all(.., test, ..)`, and an
/// `any(..)` all of whose arms do. `any(test, unix)` does not: it is production code on unix.
fn implies_test(pred: &[TokenTree]) -> bool {
    match pred {
        [TokenTree::Ident(i)] => i == "test",
        [TokenTree::Ident(i), TokenTree::Group(g)] if i == "all" => cfg_items(g.stream()).iter().any(|p| implies_test(p)),
        [TokenTree::Ident(i), TokenTree::Group(g)] if i == "any" => {
            let arms = cfg_items(g.stream());
            !arms.is_empty() && arms.iter().all(|p| implies_test(p))
        }
        _ => false,
    }
}

fn is_test_item(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        if a.path().is_ident("cfg") {
            if let syn::Meta::List(list) = &a.meta {
                let items = cfg_items(list.tokens.clone());
                return items.len() == 1 && implies_test(&items[0]);
            }
            return false;
        }
        a.path().segments.last().is_some_and(|s| s.ident == "test")
    })
}

/// The outer attributes of the expression kinds a statement starts with.
fn expr_attrs(e: &syn::Expr) -> &[syn::Attribute] {
    match e {
        syn::Expr::Await(x) => &x.attrs,
        syn::Expr::Call(x) => &x.attrs,
        syn::Expr::MethodCall(x) => &x.attrs,
        syn::Expr::Macro(x) => &x.attrs,
        syn::Expr::Block(x) => &x.attrs,
        syn::Expr::If(x) => &x.attrs,
        syn::Expr::Match(x) => &x.attrs,
        syn::Expr::Assign(x) => &x.attrs,
        syn::Expr::Path(x) => &x.attrs,
        syn::Expr::Try(x) => &x.attrs,
        syn::Expr::Unsafe(x) => &x.attrs,
        syn::Expr::ForLoop(x) => &x.attrs,
        syn::Expr::While(x) => &x.attrs,
        syn::Expr::Loop(x) => &x.attrs,
        _ => &[],
    }
}

fn type_name(t: &syn::Type) -> Option<String> {
    match t {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn path_of(func: &syn::Expr) -> Option<Vec<String>> {
    match func {
        syn::Expr::Path(p) => Some(p.path.segments.iter().map(|s| s.ident.to_string()).collect()),
        _ => None,
    }
}

fn last_ident(func: &syn::Expr) -> Option<String> {
    path_of(func).and_then(|p| p.last().cloned())
}

fn peel(e: &syn::Expr) -> &syn::Expr {
    match e {
        syn::Expr::Paren(p) => peel(&p.expr),
        syn::Expr::Group(g) => peel(&g.expr),
        syn::Expr::Try(t) => peel(&t.expr),
        other => other,
    }
}

/// A block's value: its last statement, when that is an expression without a semicolon.
fn block_value(b: &syn::Block) -> Option<&syn::Expr> {
    match b.stmts.last() {
        Some(syn::Stmt::Expr(e, None)) => Some(e),
        _ => None,
    }
}

/// Through `Box::pin(..)`, `.instrument(..)`, `{ .. }` and the like, to the future really
/// awaited.
fn see_through(e: &syn::Expr) -> &syn::Expr {
    match peel(e) {
        syn::Expr::MethodCall(mc) if TRANSPARENT_METHODS.contains(&mc.method.to_string().as_str()) => see_through(&mc.receiver),
        syn::Expr::MethodCall(mc) if !mc.args.is_empty() && TRANSPARENT_LAST_ARG_METHODS.contains(&mc.method.to_string().as_str()) => {
            see_through(&mc.args[mc.args.len() - 1])
        }
        syn::Expr::Call(c) if c.args.len() == 1 && last_ident(&c.func).is_some_and(|n| TRANSPARENT_FNS.contains(&n.as_str())) => {
            see_through(&c.args[0])
        }
        syn::Expr::Call(c) if !c.args.is_empty() && last_ident(&c.func).is_some_and(|n| TRANSPARENT_LAST_ARG_FNS.contains(&n.as_str())) => {
            see_through(&c.args[c.args.len() - 1])
        }
        syn::Expr::Block(b) => match block_value(&b.block) {
            Some(v) => see_through(v),
            None => peel(e),
        },
        other => other,
    }
}

/// Where a call starts, as the key `consumed` uses.
fn call_key(e: &syn::Expr) -> Option<(usize, usize)> {
    match e {
        syn::Expr::MethodCall(mc) => {
            let at = mc.method.span().start();
            Some((at.line, at.column))
        }
        syn::Expr::Call(c) => {
            let at = c.func.span().start();
            Some((at.line, at.column))
        }
        _ => None,
    }
}

fn is_string_literal(text: &str) -> bool {
    text.starts_with('"')
        || text.starts_with("r\"")
        || text.starts_with("r#")
        || text.starts_with("b\"")
        || text.starts_with("br")
        || text.starts_with("c\"")
}

impl Visitor<'_> {
    fn func(&self) -> String {
        self.funcs.last().cloned().unwrap_or_else(|| {
            if self.mods.is_empty() {
                "<module>".to_string()
            } else {
                format!("{}::<module>", self.mods.join("::"))
            }
        })
    }

    fn record(&mut self, line: usize, column: usize, what: String, kind: Wait, bounded: bool) {
        if self.test_depth > 0 || (kind == Wait::Async && self.bounded_depth > 0) {
            return;
        }
        self.out.found.push(Found {
            line,
            column,
            // A wait inside a macro's body (a `select!` arm, say) is tagged on its own line: the
            // statement holding the macro is the macro's, not the arm's.
            stmt: if self.token_depth > 0 { None } else { self.stmts.last().copied() },
            what,
            bounded,
            func: self.func(),
        });
    }

    fn literal(&mut self, span: proc_macro2::Span) {
        let (a, b) = (span.start(), span.end());
        self.out.literals.push(((a.line, a.column), (b.line, b.column)));
    }

    fn waits_when_awaited(&self, name: &str) -> bool {
        AWAIT_METHODS.contains(&name) || self.reg.wait_methods.contains(name)
    }

    fn blocks(&self, name: &str, zero_args: bool) -> bool {
        (zero_args && BLOCKING_ZERO_ARG.contains(&name)) || BLOCKING_ANY_ARG.contains(&name) || self.reg.blocking_methods.contains(name)
    }

    /// Record an `.await` on a call: a method `name` when `path` is `None`, else a function.
    /// `on_self`: a method called on bare `self`, which is certainly this code's own.
    fn await_on_call(&mut self, name: &str, path: Option<&[String]>, on_self: bool, line: usize, column: usize) {
        let method = path.is_none();
        let (what, bounded) = if self.reg.wait_fns.contains(name) {
            let shown = if method { format!(".{name}(..)") } else { format!("{name}(..)") };
            (format!("`{shown}.await`, a declared helper that waits"), false)
        } else if method && self.waits_when_awaited(name) {
            (format!("`.{name}(..).await`"), false)
        } else if !method && BOUNDING_FNS.contains(&name) {
            (format!("`{name}(..).await`"), true)
        } else if SPAWNING_FNS.contains(&name) || (!method && self.reg.wait_methods.contains(name)) {
            let shown = if method { format!(".{name}(..)") } else { format!("{name}(..)") };
            (format!("`{shown}.await`"), false)
        } else if self.reg.not_waits.contains(name) {
            return;
        } else if let Some(p) = path {
            match self.callee(p) {
                Callee::Local => return,
                // Only the registry can say whose it is (review finding C-16).
                Callee::Ambiguous => {
                    if self.test_depth == 0 && !self.reg.local_methods.contains(name) {
                        self.out.ambiguous_fns.push((line, name.to_string()));
                    }
                    return;
                }
                Callee::Foreign => (format!("`{}(..).await`, an async call this code does not define", p.join("::")), false),
            }
        } else if self.names.is_local_method(name) {
            // A method matched by name alone: on `self` it is this code's; on anything else it may
            // be a dependency's of the same name, and only the registry can say which.
            if !on_self && self.test_depth == 0 && !self.reg.local_methods.contains(name) {
                self.out.ambiguous.push((line, name.to_string()));
            }
            return;
        } else {
            (format!("`.{name}(..).await`, an async call this code does not define"), false)
        };
        self.record(line, column, what, Wait::Async, bounded);
    }

    /// Whose function a call through `path` reaches. The path is read through this file's `use`s:
    /// `use dep::publish; publish(..)` calls the dependency's, whatever `async fn publish` this
    /// code defines elsewhere (review finding C-16). A bare name nothing imports by name is this
    /// code's, unless the file glob-imports from a dependency and does not define the name itself.
    fn callee(&self, path: &[String]) -> Callee {
        if let Some(readings) = self.uses.readings(path) {
            return if readings.iter().all(|r| self.names.is_local_fn(r)) { Callee::Local } else { Callee::Foreign };
        }
        if !self.names.is_local_fn(path) {
            return Callee::Foreign;
        }
        if path.len() == 1 && !self.uses.fns.contains(&path[0]) && self.uses.globs_outside(self.names) {
            return Callee::Ambiguous;
        }
        Callee::Local
    }

    /// A call through `path`, not awaited where it is made, makes one of this code's futures.
    fn escapes_fn(&self, path: &[String]) -> bool {
        let readings = self.uses.readings(path).unwrap_or_else(|| vec![path.to_vec()]);
        self.callee(path) != Callee::Foreign && readings.iter().all(|r| self.names.escapes_fn(r))
    }

    /// What `segs` may stand for. A path into this crate (`crate::`, `self::`, `super::`) is read
    /// through the names any of its files binds — a segment that names one of its own modules or
    /// types stays that module; any other path through this file's `use`s, or as written when
    /// they bind nothing it starts with.
    fn readings(&self, segs: &[String]) -> Vec<Vec<String>> {
        let lead = segs.iter().take_while(|s| matches!(s.as_str(), "crate" | "self" | "super")).count();
        if lead == 0 {
            return self.uses.readings(segs).unwrap_or_else(|| vec![segs.to_vec()]);
        }
        let rest = &segs[lead..];
        let mut out = Vec::new();
        for (i, seg) in rest.iter().enumerate() {
            if self.names.local_paths.contains(seg) {
                continue;
            }
            for module in self.names.bound.get(seg).into_iter().flatten() {
                let mut reading = module.clone();
                reading.extend_from_slice(&rest[i + 1..]);
                out.push(reading);
            }
        }
        out
    }

    /// The raw lock `segs` names, read through what binds it (see [`Self::readings`]); shown as
    /// written when it is read as something else (review findings C-14, C-15).
    fn raw_lock_in(&self, segs: &[String]) -> Option<String> {
        self.readings(segs).iter().find_map(|r| {
            let raw = raw_lock(r)?;
            Some(if *r == segs { raw } else { format!("{raw} (named `{}`)", segs.join("::")) })
        })
    }

    /// `segs`, read through what binds it, is a module raw locks live in.
    fn lock_module_in(&self, segs: &[String]) -> bool {
        self.readings(segs).iter().any(|r| lock_module(r))
    }

    /// The `raw-locks` check of a `use` tree: a raw lock imported, a lock module aliased, glob-
    /// imported or — from a `pub` use — re-exported, each read through this file's bindings
    /// (`use std::sync; use sync::*`).
    fn raw_locks_in_use(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>, public: bool) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(p.ident.to_string());
                self.raw_locks_in_use(&p.tree, prefix, public);
                prefix.pop();
            }
            syn::UseTree::Name(n) => {
                let mut segs = prefix.clone();
                if n.ident != "self" {
                    segs.push(n.ident.to_string());
                }
                let line = n.ident.span().start().line;
                if let Some(raw) = self.raw_lock_in(&segs) {
                    self.out.raw_locks.push((line, raw));
                } else if public && self.lock_module_in(&segs) {
                    // Other modules, and other crates, name its locks through the re-export.
                    self.out.raw_locks.push((line, format!("{} (a re-export of a raw-lock module)", segs.join("::"))));
                }
            }
            syn::UseTree::Rename(r) => {
                let mut segs = prefix.clone();
                if r.ident != "self" {
                    segs.push(r.ident.to_string());
                }
                let line = r.ident.span().start().line;
                if let Some(raw) = self.raw_lock_in(&segs) {
                    self.out.raw_locks.push((line, raw));
                } else if self.lock_module_in(&segs) {
                    self.out.raw_locks.push((line, format!("{} as {} (a module alias)", segs.join("::"), r.rename)));
                }
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.raw_locks_in_use(t, prefix, public);
                }
            }
            syn::UseTree::Glob(g) => {
                if self.lock_module_in(prefix) {
                    self.out.raw_locks.push((g.star_token.span.start().line, format!("{}::* (a glob import)", prefix.join("::"))));
                }
            }
        }
    }

    fn item(&mut self, attrs: &[syn::Attribute], span: proc_macro2::Span, func: Option<String>, f: impl FnOnce(&mut Self)) {
        let test = is_test_item(attrs);
        if test {
            self.test_depth += 1;
            self.out.test_ranges.push((span.start().line, span.end().line));
        }
        let pushed = func.is_some();
        if let Some(name) = func {
            let prefix: Vec<&str> = self.mods.iter().map(String::as_str).chain(self.owners.last().map(String::as_str)).collect();
            self.funcs.push(if prefix.is_empty() { name } else { format!("{}::{name}", prefix.join("::")) });
        }
        // A function's contexts are its own.
        let bounded = std::mem::replace(&mut self.bounded_depth, 0);
        let bounding = std::mem::replace(&mut self.bounding_future, 0);
        f(self);
        self.bounded_depth = bounded;
        self.bounding_future = bounding;
        if pushed {
            self.funcs.pop();
        }
        if test {
            self.test_depth -= 1;
        }
    }

    /// A wait-shaped future handed to a call (`run_async(tx.send(..))`, `traced(child.kill())`) is
    /// awaited by that call, wherever it is: a wait where it is made.
    fn escaping_arguments<'a>(&mut self, args: impl Iterator<Item = &'a syn::Expr>) {
        for arg in args {
            let syn::Expr::MethodCall(mc) = see_through(arg) else { continue };
            let name = mc.method.to_string();
            let at = mc.method.span().start();
            let zero_arg_blocking = mc.args.is_empty() && BLOCKING_ZERO_ARG.contains(&name.as_str());
            if self.consumed.contains(&(at.line, at.column)) || zero_arg_blocking || ESCAPING_METHODS.contains(&name.as_str()) {
                continue; // a blocking call or an escaping-method future is recorded where it is visited
            }
            if self.waits_when_awaited(&name) {
                self.consumed.insert((at.line, at.column));
                self.record(at.line, at.column, format!("a `.{name}(..)` future, handed to a call that awaits it"), Wait::Async, false);
            }
        }
    }

    /// Run `f` as a spawned task: no timeout around the spawn bounds what it runs.
    fn unbounded(&mut self, f: impl FnOnce(&mut Self)) {
        let bounded = std::mem::replace(&mut self.bounded_depth, 0);
        let bounding = std::mem::replace(&mut self.bounding_future, 0);
        f(self);
        self.bounded_depth = bounded;
        self.bounding_future = bounding;
    }

    /// Walk a macro's tokens, which `syn` leaves opaque, for the waits inside them. The futures
    /// a `select!` branch names (between its `=` and its `=>`), a `join!`'s arguments and a
    /// timeout's arguments are that wait's own, so a call there that builds a future is not a
    /// second, blocking one.
    fn scan_tokens(&mut self, ts: TokenStream, mode: Scan) {
        let tts: Vec<TokenTree> = ts.into_iter().collect();
        let mut in_future = matches!(mode, Scan::Futures | Scan::TimeoutArgs);
        let select = mode == Scan::Select;
        let mut i = 0;
        while i < tts.len() {
            match &tts[i] {
                TokenTree::Literal(l) => {
                    if is_string_literal(&l.to_string()) {
                        self.literal(l.span());
                    }
                }
                TokenTree::Group(g) => {
                    let prev_ident = |k: usize| match k.checked_sub(1).and_then(|j| tts.get(j)) {
                        Some(TokenTree::Ident(id)) => Some(id.to_string()),
                        _ => None,
                    };
                    let is_async_block = g.delimiter() == Delimiter::Brace
                        && (prev_ident(i).as_deref() == Some("async")
                            || (prev_ident(i).as_deref() == Some("move") && prev_ident(i - 1).as_deref() == Some("async")));
                    match g.delimiter() {
                        Delimiter::Parenthesis => {
                            let callee = prev_ident(i);
                            if callee.as_deref().is_some_and(|c| BOUNDING_FNS.contains(&c)) {
                                self.scan_tokens(g.stream(), Scan::TimeoutArgs);
                            } else if callee.as_deref().is_some_and(|c| SPAWNING_FNS.contains(&c)) {
                                let stream = g.stream();
                                self.unbounded(|v| v.scan_tokens(stream, Scan::Code));
                            } else if mode == Scan::TimeoutArgs {
                                self.scan_tokens(g.stream(), Scan::TimeoutArgs);
                            } else {
                                self.scan_tokens(g.stream(), if in_future { Scan::Futures } else { Scan::Code });
                            }
                        }
                        Delimiter::Brace if is_async_block && mode == Scan::TimeoutArgs => {
                            self.bounded_depth += 1;
                            self.scan_tokens(g.stream(), Scan::Code);
                            self.bounded_depth -= 1;
                        }
                        _ => self.scan_tokens(g.stream(), Scan::Code),
                    }
                }
                TokenTree::Ident(id) => {
                    let name = id.to_string();
                    let at = id.span().start();
                    if is_path_start(&tts, i) {
                        self.token_path_checks(&tts, i, at.line);
                    }
                    let after_dot = i > 0 && matches!(&tts[i - 1], TokenTree::Punct(p) if p.as_char() == '.');
                    let next = tts.get(i + 1);
                    if matches!(next, Some(TokenTree::Punct(p)) if p.as_char() == '!') {
                        if let Some(TokenTree::Group(g)) = tts.get(i + 2) {
                            if WAITING_MACROS.contains(&name.as_str()) {
                                self.record(at.line, at.column, format!("`{name}!`"), Wait::Async, false);
                            }
                            self.scan_tokens(g.stream(), Scan::for_macro(&name));
                            i += 3;
                            continue;
                        }
                    }
                    if name == "await" && after_dot {
                        self.token_await(&tts, i - 1, at.line, at.column);
                    } else if let Some(TokenTree::Group(args)) = next {
                        if args.delimiter() == Delimiter::Parenthesis {
                            let awaited = awaited_after(&tts, i + 2);
                            if after_dot && !awaited && !in_future && !TRANSPARENT_METHODS.contains(&name.as_str()) {
                                if BOUNDED_BLOCKING.contains(&name.as_str()) {
                                    self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, true);
                                } else if self.blocks(&name, args.stream().is_empty()) {
                                    self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, false);
                                }
                            } else if !after_dot && name == "block_on" {
                                self.record(at.line, at.column, "`block_on(..)`".to_string(), Wait::Blocking, false);
                            } else if !awaited && self.reg.wait_fns.contains(&name) {
                                let shown = if after_dot { format!(".{name}(..)") } else { format!("{name}(..)") };
                                self.record(at.line, at.column, format!("`{shown}`, a declared helper that waits"), Wait::Blocking, false);
                            }
                        }
                    }
                }
                TokenTree::Punct(p) if select => {
                    let joint_before = i > 0 && matches!(&tts[i - 1], TokenTree::Punct(q) if q.spacing() == Spacing::Joint);
                    match p.as_char() {
                        '=' if p.spacing() == Spacing::Joint
                            && matches!(tts.get(i + 1), Some(TokenTree::Punct(q)) if q.as_char() == '>') =>
                        {
                            in_future = false;
                        }
                        '=' if p.spacing() == Spacing::Alone && !joint_before => in_future = true,
                        ',' => in_future = false,
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// The `raw-locks` and lock-class checks the AST pass makes on a path, made on one inside macro
    /// tokens: the path that starts at `tts[i]`, and the string literal its call is given.
    fn token_path_checks(&mut self, tts: &[TokenTree], i: usize, line: usize) {
        let (segs, after) = token_path_from(tts, i);
        if let Some(raw) = self.raw_lock_in(&segs) {
            self.out.raw_locks.push((line, raw));
        }
        let Some(TokenTree::Group(args)) = tts.get(after) else { return };
        if args.delimiter() != Delimiter::Parenthesis {
            return;
        }
        let arg_tokens: Vec<TokenTree> = args.stream().into_iter().collect();
        let commas = arg_tokens.iter().filter(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ',')).count();
        let trailing = matches!(arg_tokens.last(), Some(TokenTree::Punct(p)) if p.as_char() == ',');
        let arg_count = if arg_tokens.is_empty() { 0 } else { commas + 1 - usize::from(trailing) };
        if !is_class_call(&segs, arg_count) {
            return;
        }
        if let Some(TokenTree::Literal(l)) = arg_tokens.first() {
            let text = l.to_string();
            if is_string_literal(&text) && text.starts_with('"') && text.ends_with('"') && text.len() >= 2 {
                self.out.classes.push((l.span().start().line, text[1..text.len() - 1].to_string()));
            }
        }
    }

    /// An `.await` inside macro tokens; `dot` is the index of the `.` before it.
    fn token_await(&mut self, tts: &[TokenTree], dot: usize, line: usize, column: usize) {
        // Step back over `.instrument(..)`-like wrappers to the future really awaited.
        let mut end = dot;
        while end >= 3 {
            match (&tts[end - 1], &tts[end - 2], &tts[end - 3]) {
                (TokenTree::Group(g), TokenTree::Ident(name), TokenTree::Punct(p))
                    if g.delimiter() == Delimiter::Parenthesis
                        && p.as_char() == '.'
                        && TRANSPARENT_METHODS.contains(&name.to_string().as_str()) =>
                {
                    end -= 3;
                }
                _ => break,
            }
        }
        if end == 0 {
            return;
        }
        match &tts[end - 1] {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis && end >= 2 => match &tts[end - 2] {
                TokenTree::Ident(name) => {
                    let name = name.to_string();
                    let method = end >= 3 && matches!(&tts[end - 3], TokenTree::Punct(p) if p.as_char() == '.');
                    if method {
                        // `self.name(..)`: the receiver is the bare `self` token, not `x.self`-shaped.
                        let on_self = end >= 4
                            && matches!(&tts[end - 4], TokenTree::Ident(id) if id == "self")
                            && !(end >= 5 && matches!(&tts[end - 5], TokenTree::Punct(p) if p.as_char() == '.'));
                        self.await_on_call(&name, None, on_self, line, column);
                    } else {
                        let path = token_path(tts, end - 2);
                        self.await_on_call(&name, Some(&path), false, line, column);
                    }
                }
                _ => self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false),
            },
            // An async block's waits are scanned where they are; awaiting the block is not one
            // more. A plain block's value is a future the lint cannot see, as in the AST pass.
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {
                let before = |k: usize| match end.checked_sub(k).and_then(|j| tts.get(j)) {
                    Some(TokenTree::Ident(id)) => Some(id.to_string()),
                    _ => None,
                };
                let async_block = before(2).as_deref() == Some("async")
                    || (before(2).as_deref() == Some("move") && before(3).as_deref() == Some("async"));
                if !async_block {
                    self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false);
                }
            }
            TokenTree::Ident(name) if !is_keyword(&name.to_string()) => {
                self.record(line, column, format!("`.await` on a stored future (`{name}`)"), Wait::Async, false);
            }
            _ => self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false),
        }
    }
}

/// Is the call whose argument group ends just before `at` awaited — directly, or through
/// `.instrument(..)`-like wrappers?
fn awaited_after(tts: &[TokenTree], mut at: usize) -> bool {
    loop {
        let dot = matches!(tts.get(at), Some(TokenTree::Punct(p)) if p.as_char() == '.');
        match (dot, tts.get(at + 1), tts.get(at + 2)) {
            (true, Some(TokenTree::Ident(a)), _) if a == "await" => return true,
            (true, Some(TokenTree::Ident(m)), Some(TokenTree::Group(g)))
                if g.delimiter() == Delimiter::Parenthesis && TRANSPARENT_METHODS.contains(&m.to_string().as_str()) =>
            {
                at += 3;
            }
            _ => return false,
        }
    }
}

/// The path segments of a function call in tokens, ending at the ident at `last`.
fn token_path(tts: &[TokenTree], last: usize) -> Vec<String> {
    let mut path = Vec::new();
    let mut k = last;
    while let TokenTree::Ident(id) = &tts[k] {
        path.push(id.to_string());
        let colons = k >= 3
            && matches!(&tts[k - 1], TokenTree::Punct(p) if p.as_char() == ':')
            && matches!(&tts[k - 2], TokenTree::Punct(p) if p.as_char() == ':');
        if !colons {
            break;
        }
        k -= 3;
    }
    path.reverse();
    path
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "async"
            | "move"
            | "self"
            | "Self"
            | "super"
            | "crate"
            | "return"
            | "break"
            | "in"
            | "if"
            | "else"
            | "match"
            | "loop"
            | "while"
            | "for"
    )
}

impl<'ast> Visit<'ast> for Visitor<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        let name = f.sig.ident.to_string();
        self.item(&f.attrs, f.span(), Some(name), |v| syn::visit::visit_item_fn(v, f));
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        let name = f.sig.ident.to_string();
        self.item(&f.attrs, f.span(), Some(name), |v| syn::visit::visit_impl_item_fn(v, f));
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        let name = f.sig.ident.to_string();
        self.item(&f.attrs, f.span(), Some(name), |v| syn::visit::visit_trait_item_fn(v, f));
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        self.mods.push(m.ident.to_string());
        self.item(&m.attrs, m.span(), None, |v| syn::visit::visit_item_mod(v, m));
        self.mods.pop();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let ty = type_name(&i.self_ty).unwrap_or_else(|| "<impl>".to_string());
        let owner = match &i.trait_ {
            Some((_, path, _)) => {
                let tr = path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                format!("<{ty} as {tr}>")
            }
            None => ty,
        };
        self.owners.push(owner);
        self.item(&i.attrs, i.span(), None, |v| syn::visit::visit_item_impl(v, i));
        self.owners.pop();
    }

    fn visit_item_trait(&mut self, t: &'ast syn::ItemTrait) {
        self.owners.push(t.ident.to_string());
        self.item(&t.attrs, t.span(), None, |v| syn::visit::visit_item_trait(v, t));
        self.owners.pop();
    }

    fn visit_item_const(&mut self, c: &'ast syn::ItemConst) {
        self.item(&c.attrs, c.span(), None, |v| syn::visit::visit_item_const(v, c));
    }

    /// `#[cfg(test)] Some(x) => ..,` — a test-only match arm.
    fn visit_arm(&mut self, a: &'ast syn::Arm) {
        if is_test_item(&a.attrs) {
            let span = a.span();
            self.out.test_ranges.push((span.start().line, span.end().line));
            return;
        }
        syn::visit::visit_arm(self, a);
    }

    fn visit_item_static(&mut self, s: &'ast syn::ItemStatic) {
        self.item(&s.attrs, s.span(), None, |v| syn::visit::visit_item_static(v, s));
    }

    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        if self.test_depth == 0 && !is_test_item(&u.attrs) {
            let public = !matches!(u.vis, syn::Visibility::Inherited);
            self.raw_locks_in_use(&u.tree, &mut Vec::new(), public);
        }
    }

    /// `extern crate parking_lot as pl;` aliases a lock module like `use parking_lot as pl;`.
    fn visit_item_extern_crate(&mut self, e: &'ast syn::ItemExternCrate) {
        if let (0, false, Some((_, rename))) = (self.test_depth, is_test_item(&e.attrs), &e.rename) {
            let segs = vec![e.ident.to_string()];
            if lock_module(&segs) {
                self.out.raw_locks.push((rename.span().start().line, format!("{} as {rename} (a module alias)", e.ident)));
            }
        }
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        if self.test_depth == 0 {
            let segs: Vec<String> = p.segments.iter().map(|s| s.ident.to_string()).collect();
            if let Some(raw) = self.raw_lock_in(&segs) {
                self.out.raw_locks.push((p.span().start().line, raw));
            }
        }
        syn::visit::visit_path(self, p);
    }

    fn visit_lit_str(&mut self, l: &'ast syn::LitStr) {
        self.literal(l.span());
    }

    fn visit_lit_byte_str(&mut self, l: &'ast syn::LitByteStr) {
        self.literal(l.span());
    }

    fn visit_lit_cstr(&mut self, l: &'ast syn::LitCStr) {
        self.literal(l.span());
    }

    fn visit_stmt(&mut self, s: &'ast syn::Stmt) {
        let attrs: &[syn::Attribute] = match s {
            syn::Stmt::Local(l) => &l.attrs,
            syn::Stmt::Macro(m) => &m.attrs,
            // `#[cfg(test)] hold().await;` — syn keeps an expression statement's attributes on
            // the expression.
            syn::Stmt::Expr(e, _) => expr_attrs(e),
            syn::Stmt::Item(_) => &[],
        };
        let span = s.span();
        let test = is_test_item(attrs);
        if test {
            self.test_depth += 1;
            self.out.test_ranges.push((span.start().line, span.end().line));
        }
        self.stmts.push((span.start().line, span.end().line));
        syn::visit::visit_stmt(self, s);
        self.stmts.pop();
        if test {
            self.test_depth -= 1;
        }
    }

    fn visit_expr_async(&mut self, a: &'ast syn::ExprAsync) {
        // A timeout's future argument: this block runs under the timeout.
        let bounded = self.bounding_future > 0;
        let bounding = std::mem::replace(&mut self.bounding_future, 0);
        if bounded {
            self.bounded_depth += 1;
        }
        syn::visit::visit_expr_async(self, a);
        if bounded {
            self.bounded_depth -= 1;
        }
        self.bounding_future = bounding;
    }

    fn visit_expr_await(&mut self, e: &'ast syn::ExprAwait) {
        let line = e.await_token.span.start().line;
        let column = e.await_token.span.start().column;
        let base = see_through(&e.base);
        if let Some(key) = call_key(base) {
            self.consumed.insert(key);
        }
        match base {
            syn::Expr::MethodCall(mc) => {
                let at = mc.method.span().start();
                let on_self = matches!(see_through(&mc.receiver), syn::Expr::Path(p) if p.path.is_ident("self"));
                self.await_on_call(&mc.method.to_string(), None, on_self, at.line, at.column);
            }
            syn::Expr::Call(c) => match path_of(&c.func) {
                Some(path) => {
                    let at = c.func.span().start();
                    let name = path.last().cloned().unwrap_or_default();
                    self.await_on_call(&name, Some(&path), false, at.line, at.column);
                }
                None => self.record(line, column, "an `.await` on the result of a computed call".into(), Wait::Async, false),
            },
            syn::Expr::Path(p) => {
                let name = p.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                self.record(line, column, format!("`.await` on a stored future (`{name}`)"), Wait::Async, false);
            }
            syn::Expr::Async(_) => {}
            syn::Expr::Field(_) | syn::Expr::Index(_) | syn::Expr::Reference(_) | syn::Expr::Unary(_) => {
                self.record(line, column, "`.await` on a stored future".into(), Wait::Async, false);
            }
            _ => self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false),
        }
        syn::visit::visit_expr_await(self, e);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        let name = mc.method.to_string();
        let at = mc.method.span().start();
        if !self.consumed.contains(&(at.line, at.column)) {
            if BOUNDED_BLOCKING.contains(&name.as_str()) {
                self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, true);
            } else if self.blocks(&name, mc.args.is_empty()) {
                self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, false);
            } else if self.reg.wait_fns.contains(&name) {
                self.record(at.line, at.column, format!("`.{name}(..)`, a declared helper that waits"), Wait::Blocking, false);
            } else if ESCAPING_METHODS.contains(&name.as_str()) || self.names.escapes_method(&name) {
                self.record(at.line, at.column, format!("a `.{name}(..)` future, made here and awaited elsewhere"), Wait::Async, false);
            }
        }
        if self.reg.wait_fns.contains(&name) || name == "block_on" {
            for arg in &mc.args {
                if let Some(key) = call_key(see_through(arg)) {
                    self.consumed.insert(key);
                }
            }
        } else if !SPAWNING_FNS.contains(&name.as_str()) {
            self.escaping_arguments(mc.args.iter());
        }
        if SPAWNING_FNS.contains(&name.as_str()) {
            // `join_set.spawn(fut)`: the future is the task's.
            for arg in &mc.args {
                if let Some(key) = call_key(see_through(arg)) {
                    self.consumed.insert(key);
                }
            }
            self.visit_expr(&mc.receiver);
            self.unbounded(|v| {
                for arg in &mc.args {
                    v.visit_expr(arg);
                }
            });
            return;
        }
        syn::visit::visit_expr_method_call(self, mc);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if self.test_depth == 0 {
            if let Some(class) = class_literal(c) {
                self.out.classes.push(class);
            }
        }
        let path = path_of(&c.func).unwrap_or_default();
        let name = path.last().cloned().unwrap_or_default();
        let at = c.func.span().start();
        if name == "block_on" {
            self.record(at.line, at.column, "`block_on(..)`".into(), Wait::Blocking, false);
        } else if !self.consumed.contains(&(at.line, at.column)) && self.reg.wait_fns.contains(&name) {
            self.record(at.line, at.column, format!("`{name}(..)`, a declared helper that waits"), Wait::Blocking, false);
        } else if !self.consumed.contains(&(at.line, at.column)) && self.escapes_fn(&path) {
            self.record(at.line, at.column, format!("a future of `{name}(..)`, made here and awaited elsewhere"), Wait::Async, false);
        }
        let bounding = BOUNDING_FNS.contains(&name.as_str());
        let spawning = SPAWNING_FNS.contains(&name.as_str());
        // The future argument of a timeout, a spawn, a `block_on` or a declared helper is that
        // call's own; one passed to `drop` is never polled.
        if bounding || spawning || name == "block_on" || name == "drop" || self.reg.wait_fns.contains(&name) {
            for arg in &c.args {
                if let Some(key) = call_key(see_through(arg)) {
                    self.consumed.insert(key);
                }
            }
        } else {
            self.escaping_arguments(c.args.iter());
        }
        self.visit_expr(&c.func);
        let last = c.args.len().saturating_sub(1);
        for (i, arg) in c.args.iter().enumerate() {
            if spawning {
                self.unbounded(|v| v.visit_expr(arg));
            } else if bounding && i == last {
                // Only the future is polled under the timeout; the other arguments are
                // evaluated before it exists.
                self.bounding_future += 1;
                self.visit_expr(arg);
                self.bounding_future -= 1;
            } else {
                self.visit_expr(arg);
            }
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.test_depth > 0 {
            return;
        }
        let name = m.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        if WAITING_MACROS.contains(&name.as_str()) {
            let at = m.path.segments.last().map(|s| s.ident.span().start()).unwrap_or_else(|| m.span().start());
            self.record(at.line, at.column, format!("`{name}!`"), Wait::Async, false);
        }
        self.token_depth += 1;
        self.scan_tokens(m.tokens.clone(), Scan::for_macro(&name));
        self.token_depth -= 1;
    }
}

/// `lock_order::holding("processor", ..)`, `Mutex::new("store-lock", ..)`, … → the class, if it
/// is a string literal.
fn class_literal(c: &syn::ExprCall) -> Option<(usize, String)> {
    let path = path_of(&c.func)?;
    if !is_class_call(&path, c.args.len()) {
        return None;
    }
    match c.args.first()? {
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some((s.span().start().line, s.value())),
        _ => None,
    }
}

/// Does a call to `path` with `args` arguments name a lock-order class as its first argument?
fn is_class_call(path: &[String], args: usize) -> bool {
    let Some(last) = path.last().map(String::as_str) else { return false };
    matches!(last, "holding" | "holding_in" | "waits_on" | "waits_on_in" | "hold" | "scope" | "acquire")
        || (matches!(last, "new" | "with_default")
            && path.len() >= 2
            && matches!(path[path.len() - 2].as_str(), "Mutex" | "RwLock")
            && args == 2)
}

/// Is `tts[i]` the first segment of a path — not one after `::`?
fn is_path_start(tts: &[TokenTree], i: usize) -> bool {
    !(i >= 2
        && matches!(&tts[i - 1], TokenTree::Punct(p) if p.as_char() == ':')
        && matches!(&tts[i - 2], TokenTree::Punct(p) if p.as_char() == ':'))
}

/// The path starting at the identifier `tts[i]`: its segments, and the index just past it. A
/// turbofish (`::<`) ends it.
fn token_path_from(tts: &[TokenTree], i: usize) -> (Vec<String>, usize) {
    let mut segs = Vec::new();
    let mut k = i;
    while let Some(TokenTree::Ident(id)) = tts.get(k) {
        segs.push(id.to_string());
        let colons = matches!(tts.get(k + 1), Some(TokenTree::Punct(p)) if p.as_char() == ':')
            && matches!(tts.get(k + 2), Some(TokenTree::Punct(p)) if p.as_char() == ':');
        if !colons || !matches!(tts.get(k + 3), Some(TokenTree::Ident(_))) {
            return (segs, k + 1);
        }
        k += 3;
    }
    (segs, k)
}

/// Lock types that must be built through `lock-order`'s wrappers (`raw-locks`).
const RAW_LOCKS: &[(&str, &str)] = &[
    ("std::sync", "Mutex"),
    ("std::sync", "RwLock"),
    ("tokio::sync", "Mutex"),
    ("tokio::sync", "RwLock"),
    ("parking_lot", "Mutex"),
    ("parking_lot", "RwLock"),
    // A raw condvar waits on a raw mutex's guard, so it is the same escape (Store no-hang §14.2).
    ("std::sync", "Condvar"),
    ("parking_lot", "Condvar"),
    // parking_lot's other locks, and its constructors that name no type (review finding C-14).
    ("parking_lot", "ReentrantMutex"),
    ("parking_lot", "FairMutex"),
    ("parking_lot", "const_mutex"),
    ("parking_lot", "const_fair_mutex"),
    ("parking_lot", "const_reentrant_mutex"),
    ("parking_lot", "const_rwlock"),
    // The generic locks parking_lot's are instances of.
    ("lock_api", "Mutex"),
    ("lock_api", "RwLock"),
    ("lock_api", "ReentrantMutex"),
];

/// The modules raw locks live in: a glob import of one, or an alias for one, would name them past
/// this check (`use std::sync::*; Mutex::new(..)`, `use std::sync as s; s::Mutex`).
fn lock_module(segs: &[String]) -> bool {
    let joined = segs.join("::");
    let joined = joined.trim_start_matches("::");
    RAW_LOCKS.iter().any(|(m, _)| *m == joined)
}

/// `std::sync::Mutex`, `tokio::sync::RwLock`, … — or any path ending `sync::Mutex`.
fn raw_lock(segs: &[String]) -> Option<String> {
    // A path that goes on past the type — `std::sync::Mutex::new`, an associated item — names it
    // as surely as the type path does.
    (2..segs.len()).find_map(|n| raw_lock_exact(&segs[..n])).or_else(|| raw_lock_exact(segs))
}

fn raw_lock_exact(segs: &[String]) -> Option<String> {
    let n = segs.len();
    // The wrappers themselves (`lock_order::sync::Mutex`) are what raw locks are replaced by.
    if n < 2 || segs[0] == "lock_order" {
        return None;
    }
    let (module, item) = (segs[..n - 1].join("::"), segs[n - 1].as_str());
    // Exactly `std::sync`, `tokio::sync`, `parking_lot`, `lock_api` (or `::std::sync`), or a bare
    // `sync::` that no `use` in the file binds — the caller reads a bound one through its `use`
    // (`use lock_order::sync; sync::Mutex` is the wrapper), and an alias (`use tokio::sync as ts`)
    // is refused where it is made.
    let module = module.trim_start_matches("::");
    let hit = RAW_LOCKS.iter().any(|(m, i)| *i == item && (module == *m || module == "sync"));
    hit.then(|| segs.join("::"))
}

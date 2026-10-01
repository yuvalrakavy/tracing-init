//! Finding the waits in one parsed file: what each is, the statement and function it sits in,
//! and which lock guards are held across it.

use std::collections::BTreeSet;

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

/// Acquisitions whose result is a guard.
const GUARD_METHODS: &[&str] = &[
    "lock",
    "read",
    "write",
    "lock_owned",
    "read_owned",
    "write_owned",
    "blocking_lock",
    "blocking_read",
    "blocking_write",
    "blocking_lock_owned",
];

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

/// Functions whose `.await` waits on a task they start, and whose future argument is that task.
const SPAWNING_FNS: &[&str] = &["spawn", "spawn_blocking", "spawn_local"];

/// Functions that bound the async waits inside their arguments.
const BOUNDING_FNS: &[&str] = &["timeout", "timeout_at"];

/// Macros that are each one wait on the futures they are given.
pub const WAITING_MACROS: &[&str] = &["select", "join", "try_join"];

/// Wrappers an `.await` sees through: `f().instrument(span).await` awaits `f()`.
const TRANSPARENT_METHODS: &[&str] = &["instrument", "in_current_span", "boxed", "fuse"];
const TRANSPARENT_FNS: &[&str] = &["pin"];

/// Names too common to read a local `async fn` into: a sync `map.get(k)` is not a future.
const COMMON_NAMES: &[&str] = &[
    "new", "get", "set", "insert", "remove", "push", "pop", "clear", "len", "is_empty", "send", "recv", "read",
    "write", "lock", "run", "start", "stop", "close", "flush", "next", "call", "execute", "handle", "update",
    "load", "save", "init", "connect", "open", "wait", "apply", "build", "from", "into", "clone", "drop",
];

/// Every `async fn` (and every non-async fn) this code defines, by name.
#[derive(Debug, Default)]
pub struct Names {
    pub local_async: BTreeSet<String>,
    pub local_sync: BTreeSet<String>,
}

impl Names {
    pub fn collect(&mut self, file: &syn::File) {
        struct C<'a>(&'a mut Names);
        impl<'ast> Visit<'ast> for C<'_> {
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

    /// A call to `name` that is not awaited where it is made builds a future that escapes.
    fn makes_escaping_future(&self, name: &str) -> bool {
        self.local_async.contains(name) && !self.local_sync.contains(name) && !COMMON_NAMES.contains(&name)
    }
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
    /// A lock acquisition, whose result may be held as a guard.
    pub guard: bool,
    /// The enclosing function, `Type::method` or `function`.
    pub func: String,
    deferred: usize,
}

/// What a guard held across a wait is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardOf {
    /// The wait at this index of [`FileScan::found`].
    Site(usize),
    /// A call to a function the registry declares returns a guard of this row (`guard-fns`).
    Key(String),
}

/// One file's waits, and the guards held across them: `(guard, guard's line, wait index)`.
#[derive(Debug, Default)]
pub struct FileScan {
    pub found: Vec<Found>,
    pub edges: Vec<(GuardOf, usize, usize)>,
    /// Line ranges of test items, whose tags are not production tags.
    pub test_ranges: Vec<(usize, usize)>,
}

pub fn scan(file: &syn::File, reg: &Registry, names: &Names) -> FileScan {
    let mut v = Visitor {
        reg,
        names,
        test_depth: 0,
        bounded_depth: 0,
        deferred_depth: 0,
        block_depth: 0,
        token_depth: 0,
        stmts: Vec::new(),
        consumed: BTreeSet::new(),
        funcs: Vec::new(),
        owners: Vec::new(),
        guards: Vec::new(),
        out: FileScan::default(),
    };
    v.visit_file(file);
    v.out
}

struct Guard {
    of: GuardOf,
    line: usize,
    name: Option<String>,
    block_depth: usize,
    deferred: usize,
}

struct Visitor<'r> {
    reg: &'r Registry,
    names: &'r Names,
    test_depth: usize,
    bounded_depth: usize,
    /// Inside a closure or an `async` block: run later, so not under the guards held here.
    deferred_depth: usize,
    block_depth: usize,
    /// Inside a macro's tokens.
    token_depth: usize,
    stmts: Vec<(usize, usize)>,
    /// `(line, column)` of calls whose future is awaited, bounded or spawned where it is made.
    consumed: BTreeSet<(usize, usize)>,
    funcs: Vec<String>,
    /// The `impl` or `trait` being visited, for `Type::method`.
    owners: Vec<String>,
    guards: Vec<Guard>,
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
    /// Arguments that are futures (a `join!`'s, a `timeout`'s).
    Futures,
}

impl Scan {
    fn for_macro(name: &str) -> Scan {
        match name {
            "select" => Scan::Select,
            "join" | "try_join" => Scan::Futures,
            _ => Scan::Code,
        }
    }
}

fn attr_is_cfg_test(attr: &syn::Attribute) -> bool {
    fn idents(ts: TokenStream, out: &mut Vec<String>) {
        for tt in ts {
            match tt {
                TokenTree::Ident(i) => out.push(i.to_string()),
                TokenTree::Group(g) => idents(g.stream(), out),
                _ => {}
            }
        }
    }
    if !attr.path().is_ident("cfg") {
        return false;
    }
    let syn::Meta::List(list) = &attr.meta else { return false };
    let mut words = Vec::new();
    idents(list.tokens.clone(), &mut words);
    words.iter().any(|w| w == "test") && !words.iter().any(|w| w == "not")
}

fn is_test_item(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| attr_is_cfg_test(a) || a.path().segments.last().is_some_and(|s| s.ident == "test"))
}

fn last_ident(func: &syn::Expr) -> Option<String> {
    match func {
        syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn peel(e: &syn::Expr) -> &syn::Expr {
    match e {
        syn::Expr::Paren(p) => peel(&p.expr),
        syn::Expr::Group(g) => peel(&g.expr),
        syn::Expr::Try(t) => peel(&t.expr),
        other => other,
    }
}

/// Through `Box::pin(..)`, `.instrument(..)` and the like, to the future really awaited.
fn see_through(e: &syn::Expr) -> &syn::Expr {
    match peel(e) {
        syn::Expr::MethodCall(mc) if TRANSPARENT_METHODS.contains(&mc.method.to_string().as_str()) => see_through(&mc.receiver),
        syn::Expr::Call(c) if c.args.len() == 1 && last_ident(&c.func).is_some_and(|n| TRANSPARENT_FNS.contains(&n.as_str())) => {
            see_through(&c.args[0])
        }
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

/// `let g = m.lock().unwrap();` holds a guard; `let n = m.lock().unwrap().len();` does not.
fn init_is_guard(e: &syn::Expr) -> bool {
    match peel(e) {
        syn::Expr::Await(a) => init_is_guard(&a.base),
        syn::Expr::MethodCall(mc) => {
            let name = mc.method.to_string();
            if GUARD_METHODS.contains(&name.as_str()) {
                return true;
            }
            matches!(name.as_str(), "unwrap" | "expect" | "unwrap_or_else" | "map_err" | "ok" | "change_context")
                && init_is_guard(&mc.receiver)
        }
        syn::Expr::Call(c) => {
            // `timeout(d, m.lock()).await` acquires a guard too.
            last_ident(&c.func).is_some_and(|n| BOUNDING_FNS.contains(&n.as_str()))
                && c.args.iter().any(|a| matches!(peel(a), syn::Expr::MethodCall(mc) if GUARD_METHODS.contains(&mc.method.to_string().as_str())))
        }
        _ => false,
    }
}

fn pat_name(p: &syn::Pat) -> Option<String> {
    match p {
        syn::Pat::Ident(pi) => Some(pi.ident.to_string()),
        syn::Pat::Type(pt) => pat_name(&pt.pat),
        syn::Pat::TupleStruct(ts) if ts.elems.len() == 1 => pat_name(&ts.elems[0]),
        _ => None,
    }
}

/// `drop(g);` → `g`.
fn dropped(stmt: &syn::Stmt) -> Option<String> {
    let syn::Stmt::Expr(syn::Expr::Call(c), _) = stmt else { return None };
    if last_ident(&c.func).as_deref() != Some("drop") || c.args.len() != 1 {
        return None;
    }
    match &c.args[0] {
        syn::Expr::Path(p) => p.path.get_ident().map(|i| i.to_string()),
        _ => None,
    }
}

impl Visitor<'_> {
    fn func(&self) -> String {
        self.funcs.last().cloned().unwrap_or_else(|| "<module>".to_string())
    }

    fn record(&mut self, line: usize, column: usize, what: String, kind: Wait, bounded: bool, guard: bool) {
        if self.test_depth > 0 || (kind == Wait::Async && self.bounded_depth > 0) {
            return;
        }
        let idx = self.out.found.len();
        self.out.found.push(Found {
            line,
            column,
            // A wait inside a macro's body (a `select!` arm, say) is tagged on its own line: the
            // statement holding the macro is the macro's, not the arm's.
            stmt: if self.token_depth > 0 { None } else { self.stmts.last().copied() },
            what,
            bounded,
            guard,
            func: self.func(),
            deferred: self.deferred_depth,
        });
        for g in &self.guards {
            if g.deferred == self.deferred_depth {
                self.out.edges.push((g.of.clone(), g.line, idx));
            }
        }
    }

    fn waits_when_awaited(&self, name: &str) -> bool {
        AWAIT_METHODS.contains(&name) || self.reg.wait_methods.contains(name)
    }

    fn blocks(&self, name: &str, zero_args: bool) -> bool {
        (zero_args && BLOCKING_ZERO_ARG.contains(&name))
            || BLOCKING_ANY_ARG.contains(&name)
            || self.reg.blocking_methods.contains(name)
    }

    /// Record an `.await` on a call to `name` (a method call when `method`).
    fn await_on_call(&mut self, name: &str, method: bool, line: usize, column: usize, guard: bool) {
        let (what, bounded) = if method && self.waits_when_awaited(name) {
            (format!("`.{name}(..).await`"), false)
        } else if !method && BOUNDING_FNS.contains(&name) {
            (format!("`{name}(..).await`"), true)
        } else if !method && (SPAWNING_FNS.contains(&name) || self.reg.wait_methods.contains(name)) {
            (format!("`{name}(..).await`"), false)
        } else if self.names.local_async.contains(name) || self.reg.not_waits.contains(name) {
            return;
        } else {
            let shown = if method { format!(".{name}(..)") } else { format!("{name}(..)") };
            (format!("`{shown}.await`, an async call this code does not define"), false)
        };
        self.record(line, column, what, Wait::Async, bounded, guard || (method && GUARD_METHODS.contains(&name)));
    }

    fn item(&mut self, attrs: &[syn::Attribute], span: proc_macro2::Span, func: Option<String>, f: impl FnOnce(&mut Self)) {
        let test = is_test_item(attrs);
        if test {
            self.test_depth += 1;
            self.out.test_ranges.push((span.start().line, span.end().line));
        }
        let pushed = func.is_some();
        if let Some(name) = func {
            self.funcs.push(name);
        }
        // A function's guards are its own.
        let guards = std::mem::take(&mut self.guards);
        let deferred = std::mem::replace(&mut self.deferred_depth, 0);
        f(self);
        self.guards = guards;
        self.deferred_depth = deferred;
        if pushed {
            self.funcs.pop();
        }
        if test {
            self.test_depth -= 1;
        }
    }

    fn method_name(&self, name: &syn::Ident) -> String {
        match self.owners.last() {
            Some(owner) => format!("{owner}::{name}"),
            None => name.to_string(),
        }
    }

    /// Walk a macro's tokens, which `syn` leaves opaque, for the waits inside them. The futures
    /// a `select!` branch names (between its `=` and its `=>`), a `join!`'s arguments and a
    /// `timeout`'s arguments are that wait's own, so a call there that builds a future is not a
    /// second, blocking one.
    fn scan_tokens(&mut self, ts: TokenStream, mode: Scan) {
        let tts: Vec<TokenTree> = ts.into_iter().collect();
        let mut in_future = mode == Scan::Futures;
        let select = mode == Scan::Select;
        let mut i = 0;
        while i < tts.len() {
            match &tts[i] {
                TokenTree::Group(g) => {
                    let bounded = g.delimiter() == Delimiter::Parenthesis
                        && i > 0
                        && matches!(&tts[i - 1], TokenTree::Ident(id) if BOUNDING_FNS.contains(&id.to_string().as_str()));
                    if bounded {
                        self.bounded_depth += 1;
                    }
                    self.scan_tokens(g.stream(), if bounded { Scan::Futures } else { Scan::Code });
                    if bounded {
                        self.bounded_depth -= 1;
                    }
                }
                TokenTree::Ident(id) => {
                    let name = id.to_string();
                    let at = id.span().start();
                    let after_dot = i > 0 && matches!(&tts[i - 1], TokenTree::Punct(p) if p.as_char() == '.');
                    let next = tts.get(i + 1);
                    if matches!(next, Some(TokenTree::Punct(p)) if p.as_char() == '!') {
                        if let Some(TokenTree::Group(g)) = tts.get(i + 2) {
                            if WAITING_MACROS.contains(&name.as_str()) {
                                self.record(at.line, at.column, format!("`{name}!`"), Wait::Async, false, false);
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
                            let awaited = matches!(tts.get(i + 2), Some(TokenTree::Punct(p)) if p.as_char() == '.')
                                && matches!(tts.get(i + 3), Some(TokenTree::Ident(a)) if a == "await");
                            if after_dot && !awaited && !in_future {
                                if BOUNDED_BLOCKING.contains(&name.as_str()) {
                                    self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, true, false);
                                } else if self.blocks(&name, args.stream().is_empty()) {
                                    let guard = GUARD_METHODS.contains(&name.as_str());
                                    self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, false, guard);
                                }
                            } else if !after_dot && name == "block_on" {
                                self.record(at.line, at.column, "`block_on(..)`".to_string(), Wait::Blocking, false, false);
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

    /// An `.await` inside macro tokens; `dot` is the index of the `.` before it.
    fn token_await(&mut self, tts: &[TokenTree], dot: usize, line: usize, column: usize) {
        if dot == 0 {
            return;
        }
        match &tts[dot - 1] {
            TokenTree::Group(g) if g.delimiter() == Delimiter::Parenthesis && dot >= 2 => match &tts[dot - 2] {
                TokenTree::Ident(name) => {
                    let name = name.to_string();
                    let method = dot >= 3 && matches!(&tts[dot - 3], TokenTree::Punct(p) if p.as_char() == '.');
                    self.await_on_call(&name, method, line, column, false);
                }
                _ => self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false, false),
            },
            TokenTree::Group(g) if g.delimiter() == Delimiter::Brace => {}
            TokenTree::Ident(name) if !is_keyword(&name.to_string()) => {
                self.record(line, column, format!("`.await` on a stored future (`{name}`)"), Wait::Async, false, false);
            }
            _ => self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false, false),
        }
    }
}

fn is_keyword(s: &str) -> bool {
    matches!(
        s,
        "async" | "move" | "self" | "Self" | "super" | "crate" | "return" | "break" | "in" | "if" | "else" | "match" | "loop" | "while" | "for"
    )
}

impl<'ast> Visit<'ast> for Visitor<'_> {
    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        let name = f.sig.ident.to_string();
        self.item(&f.attrs, f.span(), Some(name), |v| syn::visit::visit_item_fn(v, f));
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        let name = self.method_name(&f.sig.ident);
        self.item(&f.attrs, f.span(), Some(name), |v| syn::visit::visit_impl_item_fn(v, f));
    }

    fn visit_trait_item_fn(&mut self, f: &'ast syn::TraitItemFn) {
        let name = self.method_name(&f.sig.ident);
        self.item(&f.attrs, f.span(), Some(name), |v| syn::visit::visit_trait_item_fn(v, f));
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        self.item(&m.attrs, m.span(), None, |v| syn::visit::visit_item_mod(v, m));
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        let owner = match &*i.self_ty {
            syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        }
        .unwrap_or_else(|| "<impl>".to_string());
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

    fn visit_item_static(&mut self, s: &'ast syn::ItemStatic) {
        self.item(&s.attrs, s.span(), None, |v| syn::visit::visit_item_static(v, s));
    }

    fn visit_block(&mut self, b: &'ast syn::Block) {
        self.block_depth += 1;
        for stmt in &b.stmts {
            let before = self.out.found.len();
            self.visit_stmt(stmt);
            if let syn::Stmt::Local(local) = stmt {
                if let Some(init) = &local.init {
                    let guard = if init_is_guard(&init.expr) {
                        (before..self.out.found.len())
                            .find(|&i| self.out.found[i].guard && self.out.found[i].deferred == self.deferred_depth)
                            .map(|i| (GuardOf::Site(i), self.out.found[i].line))
                    } else {
                        // A call the registry declares returns a guard (`guard-fns`).
                        guard_fn_call(&init.expr, self.reg).map(|key| (GuardOf::Key(key), init.expr.span().start().line))
                    };
                    if let Some((of, line)) = guard {
                        self.guards.push(Guard {
                            of,
                            line,
                            name: pat_name(&local.pat),
                            block_depth: self.block_depth,
                            deferred: self.deferred_depth,
                        });
                    }
                }
            }
            if let Some(name) = dropped(stmt) {
                if let Some(at) = self.guards.iter().rposition(|g| g.name.as_deref() == Some(name.as_str())) {
                    self.guards.remove(at);
                }
            }
        }
        let depth = self.block_depth;
        self.guards.retain(|g| g.block_depth < depth);
        self.block_depth -= 1;
    }

    fn visit_stmt(&mut self, s: &'ast syn::Stmt) {
        let attrs: &[syn::Attribute] = match s {
            syn::Stmt::Local(l) => &l.attrs,
            syn::Stmt::Macro(m) => &m.attrs,
            _ => &[],
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

    fn visit_expr_closure(&mut self, c: &'ast syn::ExprClosure) {
        self.deferred_depth += 1;
        syn::visit::visit_expr_closure(self, c);
        self.deferred_depth -= 1;
    }

    fn visit_expr_async(&mut self, a: &'ast syn::ExprAsync) {
        self.deferred_depth += 1;
        syn::visit::visit_expr_async(self, a);
        self.deferred_depth -= 1;
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
                self.await_on_call(&mc.method.to_string(), true, at.line, at.column, false);
            }
            syn::Expr::Call(c) => match last_ident(&c.func) {
                Some(name) => {
                    let at = c.func.span().start();
                    let guard = init_is_guard(base);
                    self.await_on_call(&name, false, at.line, at.column, guard);
                }
                None => self.record(line, column, "an `.await` on the result of a computed call".into(), Wait::Async, false, false),
            },
            syn::Expr::Path(p) => {
                let name = p.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                self.record(line, column, format!("`.await` on a stored future (`{name}`)"), Wait::Async, false, false);
            }
            syn::Expr::Async(_) | syn::Expr::Block(_) => {}
            syn::Expr::Field(_) | syn::Expr::Index(_) | syn::Expr::Reference(_) | syn::Expr::Unary(_) => {
                self.record(line, column, "`.await` on a stored future".into(), Wait::Async, false, false);
            }
            _ => self.record(line, column, "an `.await` the lint cannot classify".into(), Wait::Async, false, false),
        }
        // An awaited `async {}` block runs here, under the guards held here.
        if let syn::Expr::Async(a) = base {
            for attr in &e.attrs {
                self.visit_attribute(attr);
            }
            syn::visit::visit_block(self, &a.block);
            return;
        }
        syn::visit::visit_expr_await(self, e);
    }

    fn visit_expr_method_call(&mut self, mc: &'ast syn::ExprMethodCall) {
        let name = mc.method.to_string();
        let at = mc.method.span().start();
        if !self.consumed.contains(&(at.line, at.column)) {
            if BOUNDED_BLOCKING.contains(&name.as_str()) {
                self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, true, false);
            } else if self.blocks(&name, mc.args.is_empty()) {
                let guard = GUARD_METHODS.contains(&name.as_str());
                self.record(at.line, at.column, format!("blocking `.{name}(..)`"), Wait::Blocking, false, guard);
            } else if ESCAPING_METHODS.contains(&name.as_str()) || self.names.makes_escaping_future(&name) {
                self.record(at.line, at.column, format!("a `.{name}(..)` future, made here and awaited elsewhere"), Wait::Async, false, false);
            }
        }
        syn::visit::visit_expr_method_call(self, mc);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        let name = last_ident(&c.func).unwrap_or_default();
        let at = c.func.span().start();
        if name == "block_on" {
            self.record(at.line, at.column, "`block_on(..)`".into(), Wait::Blocking, false, false);
        } else if !self.consumed.contains(&(at.line, at.column)) && self.names.makes_escaping_future(&name) {
            self.record(at.line, at.column, format!("a future of `{name}(..)`, made here and awaited elsewhere"), Wait::Async, false, false);
        }
        let bounding = BOUNDING_FNS.contains(&name.as_str());
        // The future argument of a timeout, a spawn or a `block_on` is that call's own.
        if bounding || SPAWNING_FNS.contains(&name.as_str()) || name == "block_on" {
            for arg in &c.args {
                if let Some(key) = call_key(see_through(arg)) {
                    self.consumed.insert(key);
                }
            }
        }
        if bounding {
            self.bounded_depth += 1;
        }
        syn::visit::visit_expr_call(self, c);
        if bounding {
            self.bounded_depth -= 1;
        }
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if self.test_depth > 0 {
            return;
        }
        let name = m.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        if WAITING_MACROS.contains(&name.as_str()) {
            let at = m.path.segments.last().map(|s| s.ident.span().start()).unwrap_or_else(|| m.span().start());
            self.record(at.line, at.column, format!("`{name}!`"), Wait::Async, false, false);
        }
        self.token_depth += 1;
        self.scan_tokens(m.tokens.clone(), Scan::for_macro(&name));
        self.token_depth -= 1;
    }
}

/// `let g = write_with_bound(..).await;` where the registry declares `write_with_bound` returns
/// a guard of some row (`guard-fns = write_with_bound: store-lock`).
fn guard_fn_call(e: &syn::Expr, reg: &Registry) -> Option<String> {
    match peel(e) {
        syn::Expr::Await(a) => guard_fn_call(&a.base, reg),
        syn::Expr::MethodCall(mc) => {
            let name = mc.method.to_string();
            reg.guard_fns.get(&name).cloned().or_else(|| {
                matches!(name.as_str(), "unwrap" | "expect" | "map_err" | "ok").then(|| guard_fn_call(&mc.receiver, reg)).flatten()
            })
        }
        syn::Expr::Call(c) => last_ident(&c.func).and_then(|n| reg.guard_fns.get(&n).cloned()),
        _ => None,
    }
}

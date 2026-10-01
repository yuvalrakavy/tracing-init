//! The order graph and the held sets — compiled in debug builds only.
//!
//! **Internal discipline.** The checker's own state sits behind plain `std` locks, never the
//! wrappers. Two of them, never held together: a held-set shard, then (after releasing it) the
//! graph. Nothing calls out while holding either — a report is built, every internal lock is
//! released, and only then is it logged and recorded, so a tracing layer that takes a wrapped lock
//! re-enters a checker that holds nothing.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::panic::Location;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, RwLock};

/// Who holds a lock: a tokio task — and, inside it, the branch being polled (see
/// [`crate::branch`]) — or, outside any task, a thread.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Context {
    task: Option<tokio::task::Id>,
    thread: Option<std::thread::ThreadId>,
    branch: u64,
}

thread_local! {
    static BRANCH: Cell<u64> = const { Cell::new(0) };
}

static NEXT: AtomicU64 = AtomicU64::new(1);

pub fn next_id() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Run `f` with the current branch set to `id`, restoring the previous one after.
pub fn in_branch<R>(id: u64, f: impl FnOnce() -> R) -> R {
    let previous = BRANCH.with(|b| b.replace(id));
    struct Restore(u64);
    impl Drop for Restore {
        fn drop(&mut self) {
            BRANCH.with(|b| b.set(self.0));
        }
    }
    let _restore = Restore(previous);
    f()
}

/// The context running now. A `block_on` inside `block_in_place` keeps its task's id (probed on
/// tokio 1.48 and 1.52.3), so a lock taken through such a bridge is seen under its task's locks.
/// A root `block_on` has no task: its thread is the context.
pub fn current() -> Context {
    let branch = BRANCH.with(|b| b.get());
    match tokio::task::try_id() {
        Some(task) => Context { task: Some(task), thread: None, branch },
        None => Context { task: None, thread: Some(std::thread::current().id()), branch },
    }
}

pub type Site = &'static Location<'static>;

#[derive(Clone, Copy, Debug)]
struct Held {
    token: u64,
    class: &'static str,
    instance: usize,
    site: Site,
}

/// What a guard carries so it releases in the context that took it, wherever it is dropped.
#[derive(Debug)]
pub struct Token {
    ctx: Context,
    token: u64,
    class: &'static str,
    instance: usize,
    site: Site,
}

const SHARDS: usize = 16;

fn shards() -> &'static [Mutex<HashMap<Context, Vec<Held>>>; SHARDS] {
    static HELD: OnceLock<[Mutex<HashMap<Context, Vec<Held>>>; SHARDS]> = OnceLock::new();
    HELD.get_or_init(|| std::array::from_fn(|_| Mutex::new(HashMap::new())))
}

fn shard(ctx: &Context) -> &'static Mutex<HashMap<Context, Vec<Held>>> {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ctx.hash(&mut h);
    &shards()[(h.finish() as usize) % SHARDS]
}

fn held_by(ctx: &Context) -> Vec<Held> {
    let map = shard(ctx).lock().unwrap_or_else(|p| p.into_inner());
    map.get(ctx).cloned().unwrap_or_default()
}

/// Where `instance` is held now, across every context — for a wedge report. Takes each shard
/// in turn, never two at once.
pub fn holders_of(instance: usize) -> Vec<Site> {
    let mut out = Vec::new();
    for s in shards() {
        let map = s.lock().unwrap_or_else(|p| p.into_inner());
        for list in map.values() {
            out.extend(list.iter().filter(|h| h.instance == instance).map(|h| h.site));
        }
    }
    out
}

#[derive(Default)]
struct Graph {
    /// from → to → (where `from` was held, where `to` was taken), first time seen.
    edges: HashMap<&'static str, HashMap<&'static str, (Site, Site)>>,
    reported: HashSet<(&'static str, &'static str)>,
}

fn graph() -> &'static RwLock<Graph> {
    static GRAPH: OnceLock<RwLock<Graph>> = OnceLock::new();
    GRAPH.get_or_init(|| RwLock::new(Graph::default()))
}

fn recorded() -> &'static Mutex<Vec<String>> {
    static CYCLES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    CYCLES.get_or_init(|| Mutex::new(Vec::new()))
}

/// Every cycle reported so far.
pub fn cycles() -> Vec<String> {
    recorded().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Every cycle reported so far, cleared.
pub fn take_cycles() -> Vec<String> {
    std::mem::take(&mut *recorded().lock().unwrap_or_else(|p| p.into_inner()))
}

/// A path `from` → … → `to` among recorded edges, if there is one.
fn path(g: &Graph, from: &'static str, to: &'static str) -> Option<Vec<(&'static str, &'static str)>> {
    let mut stack = vec![(from, Vec::new())];
    let mut seen = HashSet::new();
    while let Some((at, trail)) = stack.pop() {
        if at == to {
            return Some(trail);
        }
        if !seen.insert(at) {
            continue;
        }
        if let Some(next) = g.edges.get(at) {
            for &n in next.keys() {
                let mut t = trail.clone();
                t.push((at, n));
                stack.push((n, t));
            }
        }
    }
    None
}

/// A blocking attempt to take `class` (`instance` is the lock's address), about to wait — or a
/// wait on a consumer, which is the same thing to the order. Records held → `class` for every
/// lock the context holds, and reports the first cycle each new edge closes: before the wait,
/// which is where a deadlock would happen.
pub fn attempt(class: &'static str, instance: usize, site: Site) {
    let ctx = current();
    let held = held_by(&ctx);
    let mut reports = Vec::new();
    for h in &held {
        if h.instance == instance {
            reports.push(format!(
                "`{class}` waited on again while this context holds it (held since {}, again at {site}): it waits on itself",
                h.site
            ));
            continue;
        }
        if h.class == class {
            // Two instances of one class (two stores' locks): no order between instances is
            // checked.
            continue;
        }
        let known = graph().read().unwrap_or_else(|p| p.into_inner()).edges.get(h.class).is_some_and(|m| m.contains_key(class));
        if known {
            continue;
        }
        let mut g = graph().write().unwrap_or_else(|p| p.into_inner());
        if g.edges.get(h.class).is_some_and(|m| m.contains_key(class)) {
            continue;
        }
        let back = path(&g, class, h.class);
        g.edges.entry(h.class).or_default().insert(class, (h.site, site));
        if let Some(back) = back {
            if g.reported.insert((h.class, class)) {
                let mut order = vec![format!("`{}` → `{class}` (held at {}, waited at {site})", h.class, h.site)];
                for (a, b) in back {
                    let (sa, sb) = g.edges[a][b];
                    order.push(format!("`{a}` → `{b}` (held at {sa}, waited at {sb})"));
                }
                reports.push(format!("lock order cycle: {}", order.join("; ")));
            }
        }
    }
    report(reports);
}

/// `class` was taken (blocking or not), or a consumer began a message: now held.
pub fn acquired(class: &'static str, instance: usize, site: Site) -> Token {
    let ctx = current();
    let token = next_id();
    shard(&ctx).lock().unwrap_or_else(|p| p.into_inner()).entry(ctx).or_default().push(Held { token, class, instance, site });
    Token { ctx, token, class, instance, site }
}

/// A guard went: its lock is no longer held by the context that took it.
pub fn released(t: &Token) {
    let mut map = shard(&t.ctx).lock().unwrap_or_else(|p| p.into_inner());
    if let Some(list) = map.get_mut(&t.ctx) {
        list.retain(|h| h.token != t.token);
        if list.is_empty() {
            map.remove(&t.ctx);
        }
    }
}

/// Move a held entry to the current context: an owned guard handed to another task or thread.
pub fn adopt(t: &mut Token) {
    released(t);
    let ctx = current();
    shard(&ctx).lock().unwrap_or_else(|p| p.into_inner()).entry(ctx).or_default().push(Held {
        token: t.token,
        class: t.class,
        instance: t.instance,
        site: t.site,
    });
    t.ctx = ctx;
}

/// Every internal lock is released by the time this runs.
fn report(reports: Vec<String>) {
    for r in reports {
        tracing::error!(kind = "lock_order_cycle", cycle = %r, "lock order cycle: two locks waited on in both orders, or one waited on again while held");
        recorded().lock().unwrap_or_else(|p| p.into_inner()).push(r);
    }
}

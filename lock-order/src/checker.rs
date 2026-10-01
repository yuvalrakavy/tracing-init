//! The order graph and the held sets — compiled in debug builds only.
//!
//! **Internal discipline.** The checker's own state sits behind plain `std` locks, never the
//! wrappers. The graph is never held with any other. A held record's move between contexts (a
//! branch ending, an adoption) and its release are ordered by the migration lock, taken first;
//! under it, one held-set shard at a time, and under a shard, a record's owner cell. Nothing calls
//! out while holding any of them. A report is recorded and queued, and the
//! watchdog thread logs it: nothing is logged from inside an acquisition, so a tracing layer that
//! takes a wrapped lock is never entered with the caller's locks held.

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

fn parents() -> &'static Mutex<HashMap<u64, u64>> {
    static PARENTS: OnceLock<Mutex<HashMap<u64, u64>>> = OnceLock::new();
    PARENTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A new branch, whose parent is the branch running now: it inherits what its parent holds, not
/// what its siblings do.
pub fn new_branch() -> u64 {
    let id = next_id();
    let parent = BRANCH.with(|b| b.get());
    parents().lock().unwrap_or_else(|p| p.into_inner()).insert(id, parent);
    id
}

/// Orders a held record's move between contexts against its release, so a guard dropped while
/// its record moves always finds it: moves take it to write, releases to read.
fn migration() -> &'static RwLock<()> {
    static MIGRATION: RwLock<()> = RwLock::new(());
    &MIGRATION
}

/// A branch ended: a guard it returned is still held, now by its parent — and its owner cell says
/// so, so the guard's drop or adoption finds it there.
pub fn end_branch(id: u64) {
    let Some(parent) = parents().lock().unwrap_or_else(|p| p.into_inner()).remove(&id) else { return };
    let _moving = migration().write().unwrap_or_else(|p| p.into_inner());
    for s in shards() {
        let moved: Vec<(Context, Vec<Held>)> = {
            let mut map = s.lock().unwrap_or_else(|p| p.into_inner());
            let keys: Vec<Context> = map.keys().filter(|c| c.branch == id).copied().collect();
            keys.into_iter().filter_map(|c| map.remove(&c).map(|v| (c, v))).collect()
        };
        for (c, held) in moved {
            let to = Context { branch: parent, ..c };
            let mut map = shard(&to).lock().unwrap_or_else(|p| p.into_inner());
            for h in &held {
                *h.owner.lock().unwrap_or_else(|p| p.into_inner()) = to;
            }
            map.entry(to).or_default().extend(held);
        }
    }
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

/// The context a held record sits under now — shared by the record and its guard's token, so a
/// move (a branch ending, an adoption) is seen by the guard's drop.
type Owner = std::sync::Arc<Mutex<Context>>;

#[derive(Clone, Debug)]
struct Held {
    token: u64,
    class: &'static str,
    instance: usize,
    site: Site,
    owner: Owner,
}

/// What a guard carries so it releases where its record is, wherever it is dropped.
#[derive(Debug)]
pub struct Token {
    owner: Owner,
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

/// What `ctx` holds, and what the branches it descends from hold.
fn held_by(ctx: &Context) -> Vec<Held> {
    let mut chain = vec![*ctx];
    // Outside any branch — every task's own context — there is no branch tree to walk, and no
    // global lock to take on every acquisition.
    if ctx.branch != 0 {
        let parents = parents().lock().unwrap_or_else(|p| p.into_inner());
        let mut at = ctx.branch;
        while at != 0 {
            let Some(&up) = parents.get(&at) else { break };
            chain.push(Context { branch: up, ..*ctx });
            at = up;
        }
    }
    let mut out = Vec::new();
    for c in chain {
        let map = shard(&c).lock().unwrap_or_else(|p| p.into_inner());
        out.extend(map.get(&c).cloned().unwrap_or_default());
    }
    out
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
    /// Self-waits reported, by class and where the second wait was made.
    reported_self: HashSet<(&'static str, Site)>,
}

fn graph() -> &'static RwLock<Graph> {
    static GRAPH: OnceLock<RwLock<Graph>> = OnceLock::new();
    GRAPH.get_or_init(|| RwLock::new(Graph::default()))
}

fn recorded() -> &'static Mutex<Vec<String>> {
    static CYCLES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    CYCLES.get_or_init(|| Mutex::new(Vec::new()))
}

fn pending() -> &'static Mutex<Vec<String>> {
    static PENDING: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(Vec::new()))
}

/// Whether a cycle is on record, or `None` if the record's lock is held — for the exit handler,
/// which must neither wait nor allocate before it knows the run fails.
#[cfg_attr(test, allow(dead_code))]
pub fn has_cycles() -> Option<bool> {
    match recorded().try_lock() {
        Ok(c) => Some(!c.is_empty()),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(!p.into_inner().is_empty()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// The cycles not yet logged — for the watchdog thread.
pub fn take_pending() -> Vec<String> {
    std::mem::take(&mut *pending().lock().unwrap_or_else(|p| p.into_inner()))
}

/// Every cycle reported so far.
pub fn cycles() -> Vec<String> {
    recorded().lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Every cycle reported so far, or `None` if the record's lock is held — for the exit handler,
/// which must not wait. (Unused in this crate's own unit tests, where the handler is off.)
#[cfg_attr(test, allow(dead_code))]
pub fn try_cycles() -> Option<Vec<String>> {
    match recorded().try_lock() {
        Ok(c) => Some(c.clone()),
        Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().clone()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
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
            let first = graph().write().unwrap_or_else(|p| p.into_inner()).reported_self.insert((class, site));
            if first {
                reports.push(format!(
                    "`{class}` waited on again while this context holds it (held since {}, again at {site}): it waits on itself",
                    h.site
                ));
            }
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
    let owner: Owner = std::sync::Arc::new(Mutex::new(ctx));
    // No migration lock: a branch's records move only once its future is dropped, after its last
    // acquisition, and a task's only by `adopt`.
    shard(&ctx).lock().unwrap_or_else(|p| p.into_inner()).entry(ctx).or_default().push(Held {
        token,
        class,
        instance,
        site,
        owner: owner.clone(),
    });
    Token { owner, token, class, instance, site }
}

/// Remove `t`'s record from wherever its owner cell says it is. The migration lock is held by the
/// caller, so the record cannot move meanwhile.
fn remove_record(t: &Token) {
    let ctx = *t.owner.lock().unwrap_or_else(|p| p.into_inner());
    let mut map = shard(&ctx).lock().unwrap_or_else(|p| p.into_inner());
    if let Some(list) = map.get_mut(&ctx) {
        list.retain(|h| h.token != t.token);
        if list.is_empty() {
            map.remove(&ctx);
        }
    }
}

/// A guard went: its lock is no longer held, by whichever context holds its record now.
pub fn released(t: &Token) {
    // A record in a branch can be moved by the branch's end while this runs: order the two. One
    // outside any branch moves only by `adopt`, which holds the token exclusively — no global
    // lock on the common path.
    let in_branch = t.owner.lock().unwrap_or_else(|p| p.into_inner()).branch != 0;
    let _ordered = in_branch.then(|| migration().read().unwrap_or_else(|p| p.into_inner()));
    remove_record(t);
}

/// Move a held entry to the current context: an owned guard handed to another task or thread.
pub fn adopt(t: &mut Token) {
    let _ordered = migration().write().unwrap_or_else(|p| p.into_inner());
    remove_record(t);
    let ctx = current();
    *t.owner.lock().unwrap_or_else(|p| p.into_inner()) = ctx;
    shard(&ctx).lock().unwrap_or_else(|p| p.into_inner()).entry(ctx).or_default().push(Held {
        token: t.token,
        class: t.class,
        instance: t.instance,
        site: t.site,
        owner: t.owner.clone(),
    });
}

/// Every internal lock is released by the time this runs. The cycle is recorded now, for a test
/// harness; the watchdog thread logs it.
fn report(reports: Vec<String>) {
    if reports.is_empty() {
        return;
    }
    recorded().lock().unwrap_or_else(|p| p.into_inner()).extend(reports.iter().cloned());
    pending().lock().unwrap_or_else(|p| p.into_inner()).extend(reports);
    crate::at_exit::arm();
    crate::watchdog::ensure_thread();
}

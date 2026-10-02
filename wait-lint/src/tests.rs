use super::crate_of;
use super::*;

const ROWS: &str = "\
| Key | Kind | Waits on | Held across | Argument |
|---|---|---|---|---|
| `k` | acyclic | a lock | — | nothing waits back |
| `b` | bounded | a reply | — | 5 s; on expiry the caller fails loudly |
";

fn run_files(files: &[(&str, &str)], registry: &str) -> Report {
    let files: BTreeMap<String, String> = files.iter().map(|(p, t)| (p.to_string(), t.to_string())).collect();
    check(&files, ("docs/wait-registry.md", registry))
}

fn run(src: &str) -> Report {
    run_files(&[("src/a.rs", src)], ROWS)
}

/// Findings about the source, by line.
fn source_findings(r: &Report) -> Vec<(usize, String)> {
    r.findings.iter().filter(|f| f.file == "src/a.rs").map(|f| (f.line.unwrap_or(0), f.message.clone())).collect()
}

fn registry_findings(r: &Report) -> Vec<(Option<usize>, String)> {
    r.findings.iter().filter(|f| f.file == "docs/wait-registry.md").map(|f| (f.line, f.message.clone())).collect()
}

fn untagged_lines(r: &Report) -> Vec<usize> {
    source_findings(r).into_iter().filter(|(_, m)| m.contains("untagged wait")).map(|(l, _)| l).collect()
}

fn tags(r: &Report) -> Vec<Option<&str>> {
    r.sites.iter().map(|s| s.tag.as_deref()).collect()
}

#[test]
fn an_untagged_lock_await_is_a_finding() {
    let r = run("async fn f(m: M) {\n    let g = m.lock().await;\n}\n");
    assert_eq!(r.sites.len(), 1);
    assert_eq!(untagged_lines(&r), vec![2]);
}

#[test]
fn a_tag_on_the_line_or_directly_above_covers_the_wait() {
    let src = "\
async fn f(m: M, n: M) {
    m.lock().await.push(1); // WAIT: k
    // Why this one cannot hang:
    // WAIT: k — the prose may follow
    // (more prose)
    n.lock().await.push(2);
}
";
    let r = run(src);
    assert_eq!(tags(&r), vec![Some("k"), Some("k")]);
    assert!(source_findings(&r).is_empty(), "{:?}", r.findings);
}

#[test]
fn a_send_whose_await_is_on_the_next_line_is_one_wait_covered_by_its_statement_tag() {
    let src = "\
async fn f(tx: T) {
    // WAIT: k
    tx
        .send(1)
        .await
        .unwrap();
}
";
    let r = run(src);
    assert_eq!(r.sites.len(), 1, "{:?}", r.sites);
    assert_eq!(r.sites[0].line, 4);
    assert_eq!(tags(&r), vec![Some("k")]);
    assert!(source_findings(&r).is_empty(), "{:?}", r.findings);
}

#[test]
fn a_tag_over_several_waits_lists_one_key_per_wait_in_order() {
    let src = "\
async fn f(x: M, y: R) {
    // WAIT: k, b
    let (a, c) = (
        x.lock().await,
        y.recv().await,
    );
}
";
    let r = run(src);
    assert_eq!(tags(&r), vec![Some("k"), Some("b")]);
    assert!(source_findings(&r).is_empty(), "{:?}", r.findings);
}

#[test]
fn one_key_over_several_waits_is_refused() {
    let src = "\
async fn f(x: M, y: M) {
    // WAIT: k
    let (a, b) = (
        x.lock().await,
        y.lock().await,
    );
}
";
    let r = run(src);
    let f = source_findings(&r);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, 2);
    assert!(f[0].1.contains("1 key(s) for 2 wait(s)"), "{f:?}");
}

#[test]
fn select_is_one_wait_and_its_branch_futures_are_not_more() {
    let src = "\
async fn f(rx: R, c: C) {
    tokio::select! {
        v = rx.recv() => {}
        _ = c.cancelled() => {}
    }
}
";
    let r = run(src);
    assert_eq!(r.sites.len(), 1, "{:?}", r.sites);
    assert_eq!(r.sites[0].what, "`select!`");
    assert_eq!(untagged_lines(&r), vec![2]);
}

#[test]
fn a_wait_inside_a_select_handler_is_its_own_site() {
    let src = "\
async fn f(rx: R, m: M) {
    tokio::select! { // WAIT: k
        v = rx.recv() => {
            let g = m.lock().await;
        }
    }
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![4]);
}

#[test]
fn block_on_as_a_function_and_as_a_method_is_a_wait() {
    let src = "\
fn f(h: H) {
    futures::executor::block_on(g());
    h.block_on(g());
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![2, 3]);
}

#[test]
fn a_timeout_is_a_bounded_wait_whose_row_must_be_bounded() {
    let src = "\
async fn f(rx: R, m: M) {
    let a = tokio::time::timeout(D, rx.recv()).await; // WAIT: b
    let b = timeout(D, async { m.lock().await }).await; // WAIT: k
}
";
    let r = run(src);
    assert_eq!(r.sites.len(), 2, "the async waits inside a timeout are its own: {:?}", r.sites);
    assert!(r.sites.iter().all(|s| s.bounded));
    let f = source_findings(&r);
    assert_eq!(f.len(), 1, "{f:?}");
    assert_eq!(f[0].0, 3);
    assert!(f[0].1.contains("an `acyclic` row"), "{f:?}");
}

#[test]
fn a_blocking_call_inside_a_timeout_is_still_a_wait() {
    let src = "\
async fn f(m: StdMutex) {
    let a = timeout(D, async { m.lock().unwrap() }).await; // WAIT: b
}
";
    let r = run(src);
    assert_eq!(r.sites.len(), 2, "{:?}", r.sites);
    assert!(r.sites.iter().any(|s| s.what.contains("blocking `.lock")), "{:?}", r.sites);
}

#[test]
fn try_forms_are_not_waits_and_timeouts_by_argument_are_bounded_waits() {
    let src = "\
fn f(tx: T, rx: R, cv: C) {
    tx.try_send(1);
    rx.try_recv();
    rx.recv_timeout(D);
}
";
    let r = run(src);
    assert_eq!(r.sites.len(), 1, "{:?}", r.sites);
    assert_eq!(r.sites[0].line, 4);
    assert!(r.sites[0].bounded);
}

#[test]
fn every_await_is_accounted_for() {
    let src = "\
async fn helper(m: M) {
    m.lock().await; // WAIT: k
}

async fn f(c: Client, m: M) {
    helper(m).await;
    self.helper(m).await;
    tokio::time::sleep(D).await;
    c.fetch().await;
    async { 1 }.await;
    helper(m).instrument(span).await;
    Box::pin(helper(m)).await;
}
";
    let registry = format!("{ROWS}\n```wait-lint\nnot-waits = sleep\n```\n");
    let r = run_files(&[("src/a.rs", src)], &registry);
    let lines: Vec<usize> = r.sites.iter().map(|s| s.line).collect();
    assert_eq!(lines, vec![2, 9], "only the helper's own lock and the unknown dependency call: {:?}", r.sites);
    assert!(r.sites[1].what.contains("does not define"), "{:?}", r.sites);
    assert_eq!(untagged_lines(&r), vec![9]);
}

#[test]
fn a_stored_future_and_a_spawned_task_are_waits() {
    let src = "\
async fn f(reply_rx: O, handle: J, rx: O) {
    reply_rx.await;
    self.handle.await;
    (&mut rx).await;
    tokio::spawn(g()).await;
    spawn_blocking(h).await;
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![2, 3, 4, 5, 6]);
}

#[test]
fn a_future_made_here_and_awaited_elsewhere_is_a_wait_where_it_is_made() {
    let src = "\
async fn idle(e: E) {
    e.n.notified().await; // WAIT: k
}

async fn step<F>(fut: F) {
    fut.await; // WAIT: k
}

async fn f(e: E, n: N) {
    step(idle(e)).await;
    let notified = n.notified();
    tokio::spawn(idle(e));
}
";
    let r = run(src);
    assert_eq!(
        untagged_lines(&r),
        vec![10, 11],
        "the escaping `idle(e)` and `notified()`; the spawned one is its task's: {:?}",
        r.findings
    );
}

#[test]
fn blocking_calls_are_recognized_by_their_zero_argument_forms() {
    let src = "\
fn f(m: M, p: P, t: T, cv: C, rx: R) {
    let g = m.lock().unwrap();
    let q = p.join(\"x\");
    t.join();
    let g = cv.wait(g);
    rx.recv();
    let r = store.read(key);
    let w = rw.write().unwrap();
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![2, 4, 5, 6, 8]);
}

#[test]
fn a_wait_inside_another_macro_is_found() {
    let src = "\
async fn f(rx: R) {
    info!(\"got {}\", rx.recv().await.unwrap());
    let v = vec![m.lock().unwrap()];
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![2, 3]);
}

#[test]
fn test_code_is_exempt() {
    let src = "\
#[cfg(test)]
mod tests {
    async fn t(m: M) { m.lock().await; }
}

async fn p(m: M) {
    #[cfg(test)]
    hold(m).await;
    #[cfg(test)]
    m.lock().await;
}

#[test]
fn u() { m.lock().unwrap(); }

#[cfg(test)]
mod more;
";
    let more = "async fn t(m: M) { m.lock().await; }\n";
    let files = [("src/a.rs", src), ("src/a/more.rs", more), ("src/b_tests.rs", more), ("tests/it.rs", more)];
    let r = run_files(&files, ROWS);
    assert!(r.sites.is_empty(), "{:?}", r.sites);
}

#[test]
fn a_tag_must_name_a_row_and_every_row_must_be_named() {
    let r = run("async fn f(m: M) {\n    m.lock().await; // WAIT: nope\n}\n");
    let f = source_findings(&r);
    assert_eq!(f.len(), 1, "{f:?}");
    assert!(f[0].1.contains("names no row"), "{f:?}");
    let unused: Vec<_> = registry_findings(&r).into_iter().filter(|(_, m)| m.contains("named by no wait")).collect();
    assert_eq!(unused.len(), 2, "both unused rows: {unused:?}");
}

#[test]
fn a_tag_that_covers_no_wait_and_a_malformed_tag_are_findings() {
    let r = run("fn f() {\n    // WAIT: k\n    let x = 1;\n    let y = 2; // WAIT: <key>\n}\n");
    let f = source_findings(&r);
    assert!(f.iter().any(|(l, m)| *l == 2 && m.contains("covers no wait")), "{f:?}");
    assert!(f.iter().any(|(l, m)| *l == 4 && m.contains("malformed")), "{f:?}");
}

#[test]
fn the_registry_refuses_a_bad_kind_a_duplicate_and_a_bad_key() {
    let registry = "\
| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `a` | maybe | x | y |
| `a` | acyclic | x | y |
| `Bad Key` | acyclic | x | y |
";
    let r = run_files(&[("src/a.rs", "fn f() {}\n")], registry);
    let msgs = registry_findings(&r);
    assert!(msgs.iter().any(|(l, m)| *l == Some(3) && m.contains("`acyclic` or `bounded`")), "{msgs:?}");
    assert!(msgs.iter().any(|(l, m)| *l == Some(4) && m.contains("second row")), "{msgs:?}");
    assert!(msgs.iter().any(|(l, m)| *l == Some(5) && m.contains("not a key")), "{msgs:?}");
}

#[test]
fn declared_wait_methods_and_blocking_methods_are_waits() {
    let src = "\
async fn f(c: Client, tx: SyncSender) {
    c.publish(t, q, r, p).await;
    tx.send(1);
}
";
    let registry = format!("{ROWS}\n```wait-lint\nwait-methods = publish\nblocking-methods = send\n```\n");
    let r = run_files(&[("src/a.rs", src)], &registry);
    assert_eq!(untagged_lines(&r), vec![2, 3]);
}

#[test]
fn an_unparsable_file_is_a_finding_not_a_silence() {
    let r = run("fn f( {\n");
    assert!(source_findings(&r).iter().any(|(_, m)| m.contains("cannot parse")), "{:?}", r.findings);
}

// The waiters block.

const ONE_WAIT: &str = "impl S {\n    async fn f(m: M) {\n        m.lock().await; // WAIT: k\n    }\n}\n";
const K_ONLY: &str = "\
| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `k` | acyclic | a lock | nothing waits back |
";

#[test]
fn a_missing_waiters_block_is_a_finding_and_the_report_renders_it() {
    let r = run_files(&[("src/a.rs", ONE_WAIT)], K_ONLY);
    assert!(registry_findings(&r).iter().any(|(_, m)| m.contains("no `wait-lint-waiters` block")), "{:?}", r.findings);
    assert_eq!(r.waiters_block, "```wait-lint-waiters\nk src/a.rs S::f\n```\n");
}

#[test]
fn a_new_waiter_and_a_gone_waiter_are_findings() {
    let registry = format!("{K_ONLY}\n```wait-lint-waiters\nk src/a.rs S::gone\n```\n");
    let r = run_files(&[("src/a.rs", ONE_WAIT)], &registry);
    let msgs = registry_findings(&r);
    assert!(msgs.iter().any(|(_, m)| m.contains("now has a wait in `S::f`")), "{msgs:?}");
    assert!(msgs.iter().any(|(l, m)| l.is_some() && m.contains("S::gone") && m.contains("holds no such wait")), "{msgs:?}");
}

#[test]
fn a_current_waiters_block_is_clean_and_rewrites_in_place() {
    let block = "```wait-lint-waiters\nk src/a.rs S::f\n```\n";
    let registry = format!("{K_ONLY}\n{block}\nTrailing prose.\n");
    let r = run_files(&[("src/a.rs", ONE_WAIT)], &registry);
    assert!(r.findings.is_empty(), "{:?}", r.findings);
    let stale = registry.replace("S::f", "S::old");
    assert_eq!(registry::with_waiters(&stale, &r.waiters_block), registry);
}

// The re-gate's probes (Codex, on 2de2c4a): each was silent, and is not.

#[test]
fn a_test_helper_named_like_a_dependency_does_not_excuse_it() {
    let src = "\
#[cfg(test)]
async fn pending() {}

async fn f() {
    std::future::pending::<()>().await;
    pending().await;
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![5, 6], "{:?}", r.sites);
}

#[test]
fn a_local_path_is_delegated_and_a_dependency_path_is_not() {
    let src = "\
mod helpers {
    pub async fn settle() {}
}
async fn f() {
    helpers::settle().await;
    crate::helpers::settle().await;
    tokio::settle().await;
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![7], "{:?}", r.sites);
}

#[test]
fn a_block_s_value_is_what_is_awaited() {
    let src = "\
async fn f() {
    { std::future::pending::<()>() }.await;
    id!({ std::future::pending::<()>() }.await);
}
";
    let r = run(src);
    // Review finding (Codex, 2026-10-02): the macro form used to be skipped, so an infinite wait
    // inside a macro needed no tag. Both forms are unclassified waits now.
    assert_eq!(untagged_lines(&r), vec![2, 3], "the AST block and the macro's: {:?}", r.sites);
}

#[test]
fn an_async_block_awaited_inside_a_macro_is_not_a_wait_of_its_own() {
    let r = run("async fn f() {\n    id!(async { 1 }.await);\n    id!(async move { 2 }.await);\n}\n");
    assert!(r.sites.is_empty(), "{:?}", r.sites);
}

/// Review finding (Codex, 2026-10-02): macro tokens skipped the raw-lock and class checks, and an
/// expression path (`std::sync::Mutex::new(0)`, no type named) escaped the raw-lock check anywhere.
#[test]
fn raw_locks_and_classes_inside_macros_and_expressions_are_checked() {
    let src = "\
fn f() {
    let v = vec![std::sync::Mutex::new(0)];
    let w = vec![lock_order::sync::Mutex::new(\"misspelled-class\", 0)];
    let x = std::sync::Mutex::new(0);
    let y = vec![lock_order::sync::Mutex::new(\"k\", 0)];
    let z = vec![lock_order::holding(\"nope\", g())];
}
";
    let forbid = format!("{ROWS}\n```wait-lint\nraw-locks = forbid\n```\n");
    let r = run_files(&[("src/a.rs", src)], &forbid);
    let raw: Vec<usize> = source_findings(&r).into_iter().filter(|(_, m)| m.contains("raw lock")).map(|(l, _)| l).collect();
    assert_eq!(raw, vec![2, 4], "{:?}", r.findings);
    let bad: Vec<usize> = source_findings(&r).into_iter().filter(|(_, m)| m.contains("is no row")).map(|(l, _)| l).collect();
    assert_eq!(bad, vec![3, 6], "{:?}", r.findings);
}

#[test]
fn only_a_cfg_that_implies_test_is_test_code() {
    let src = "\
#[cfg(any(test, unix))]
async fn f() { std::future::pending::<()>().await; }
#[cfg(all(test, unix))]
async fn g() { std::future::pending::<()>().await; }
#[cfg(any(test, all(test, unix)))]
async fn h() { std::future::pending::<()>().await; }
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![2], "{:?}", r.sites);
}

#[test]
fn a_timeout_bounds_its_future_and_not_its_eager_arguments_or_a_spawned_task() {
    let src = "\
async fn f(m: M) {
    let a = timeout(std::future::pending().await, async {}).await; // WAIT: b
    let b = timeout(D, async { tokio::spawn(async { m.lock().await; }); }).await; // WAIT: b
}
";
    let r = run(src);
    let lines: Vec<usize> = r.sites.iter().map(|s| s.line).collect();
    assert_eq!(lines, vec![2, 2, 3, 3], "the eager pending and the spawned lock are waits: {:?}", r.sites);
}

#[test]
fn a_tag_inside_a_string_is_text_and_one_in_a_doc_comment_is_refused() {
    let src = "\
async fn f(rx: R) {
    let s = r#\"// WAIT: k;\"#; rx.recv().await;
    let t = \"// WAIT: k\"; rx.recv().await;
}
/// WAIT: k
async fn g(rx: R) {
    rx.recv().await;
}
";
    let r = run(src);
    assert_eq!(untagged_lines(&r), vec![2, 3, 7], "{:?}", r.findings);
    assert!(source_findings(&r).iter().any(|(l, m)| *l == 5 && m.contains("doc comment")), "{:?}", r.findings);
}

#[test]
fn a_waiter_names_its_inline_module_and_its_trait() {
    let src = "\
mod a {
    async fn f(m: M) { m.lock().await; } // WAIT: k
}
mod b {
    async fn f(m: M) { m.lock().await; } // WAIT: k
}
impl Run for S {
    async fn f(m: M) { m.lock().await; } // WAIT: k
}
";
    let r = run_files(&[("src/a.rs", src)], K_ONLY);
    assert_eq!(r.waiters_block, "```wait-lint-waiters\nk src/a.rs <S as Run>::f\nk src/a.rs a::f\nk src/a.rs b::f\n```\n");
    // And the block reads back: a waiter's function may contain spaces.
    let registry = format!("{K_ONLY}\n{}", r.waiters_block);
    let again = run_files(&[("src/a.rs", src)], &registry);
    assert!(again.findings.is_empty(), "{:?}", again.findings);
}

#[test]
fn a_row_must_say_what_it_waits_on_and_why_and_a_bounded_row_what_expiry_does() {
    let registry = "\
| Key | Kind | Waits on | Held across | Argument |
|---|---|---|---|---|
| `a` | acyclic |  | — | why |
| `b` | acyclic | x | — |  |
| `c` | bounded | x | — | 5 s, and then something |
| `d` | bounded | x | — | 5 s; on expiry the caller fails loudly |
";
    let r = run_files(&[("src/a.rs", "fn f() {}\n")], registry);
    let msgs = registry_findings(&r);
    assert!(msgs.iter().any(|(l, m)| *l == Some(3) && m.contains("what it waits on")), "{msgs:?}");
    assert!(msgs.iter().any(|(l, m)| *l == Some(4) && m.contains("no argument")), "{msgs:?}");
    assert!(msgs.iter().any(|(l, m)| *l == Some(5) && m.contains("on expiry")), "{msgs:?}");
    assert!(!msgs.iter().any(|(l, m)| *l == Some(6) && !m.contains("named by no wait")), "{msgs:?}");
}

#[test]
fn futures_in_future_position_are_not_invented_waits() {
    let src = "\
async fn idle() {}
async fn f(rx: R) {
    tokio::select! { // WAIT: k
        v = (rx.recv()) => {}
    }
    id!(rx.recv().boxed().await);
    drop(idle());
}
";
    let r = run(src);
    let whats: Vec<(usize, &str)> = r.sites.iter().map(|s| (s.line, s.what.as_str())).collect();
    assert_eq!(whats, vec![(3, "`select!`"), (6, "`.recv(..).await`")], "{:?}", r.sites);
}

// Version 5: declared helpers, per-crate names, futures handed to calls, raw locks.

#[test]
fn a_call_to_a_declared_helper_is_a_wait_and_its_future_argument_is_its_own() {
    let src = "\
fn f(tx: T) {
    run_async(async { 1 });
    RhaiStore::run_async(tx.send(2));
    self.run_async(x);
    wrap! { run_async(x); }
}
";
    let registry = format!("{ROWS}\n```wait-lint\nwait-fns = run_async\n```\n");
    let r = run_files(&[("src/a.rs", src)], &registry);
    assert_eq!(untagged_lines(&r), vec![2, 3, 4, 5], "a helper inside a macro too: {:?}", r.sites);
    assert!(r.sites.iter().all(|s| s.what.contains("declared helper")), "the send is the helper's, not a second wait: {:?}", r.sites);
}

#[test]
fn a_wait_shaped_future_handed_to_a_call_is_a_wait_where_it_is_made() {
    let src = "\
async fn traced<F>(f: F) { f.await; } // WAIT: k
async fn g(tx: T, child: C) {
    traced(tx.send(2)).await;
    traced(child.kill()).await;
    log(tx.len());
}
";
    let registry = format!("{ROWS}\n```wait-lint\nwait-methods = kill\n```\n");
    let r = run_files(&[("src/a.rs", src)], &registry);
    assert_eq!(untagged_lines(&r), vec![3, 4], "{:?}", r.sites);
}

#[test]
fn names_are_per_crate_and_a_trait_impl_excuses_nothing() {
    let server = "\
impl StoreService for Server {
    async fn execute(&self) {}
    async fn select_store(&self) {}
}
impl Server {
    async fn settle_pending(&self) {}
}
";
    let client = "\
async fn f(client: C, s: Server) {
    client.execute(req).await;
    s.settle_pending().await;
}
";
    let own = "\
async fn g(s: Server) {
    s.settle_pending().await;
}
";
    let in_process = "\
async fn h(svc: Server) {
    svc.select_store(req).await;
}
";
    let files = [
        ("store_server/src/services.rs", server),
        ("ht_server/src/client.rs", client),
        ("store_server/src/own.rs", own),
        ("store_server/src/websocket.rs", in_process),
    ];
    let r = run_files(&files, ROWS);
    let at: Vec<(String, usize)> = r.sites.iter().map(|s| (s.file.clone(), s.line)).collect();
    assert_eq!(
        at,
        vec![("ht_server/src/client.rs".to_string(), 2), ("ht_server/src/client.rs".to_string(), 3)],
        "another crate's methods excuse nothing; the own crate's — inherent or trait impl — do: {:?}",
        r.sites
    );
}

#[test]
fn select_biased_is_one_wait() {
    let r = run("async fn f(rx: R) {\n    futures::select_biased! { v = rx.recv() => {} }\n}\n");
    assert_eq!(r.sites.len(), 1, "{:?}", r.sites);
    assert_eq!(r.sites[0].what, "`select_biased!`");
}

#[test]
fn an_example_fence_in_the_registry_is_not_read() {
    let registry = format!(
        "{ROWS}\nAn example, not a row:\n\n```text\n| Key | Kind | Waits on | Argument |\n|---|---|---|---|\n| `ex` | acyclic | x | y |\n```\n"
    );
    let r = run_files(&[("src/a.rs", "fn f() {}\n")], &registry);
    assert!(!registry_findings(&r).iter().any(|(_, m)| m.contains("`ex`")), "{:?}", r.findings);
}

#[test]
fn a_raw_lock_is_a_finding_when_forbidden() {
    let src = "\
use std::sync::{Arc, Mutex};
use tokio::sync::RwLock as R;
static S: std::sync::RwLock<u8> = std::sync::RwLock::new(0);
fn f() -> tokio::sync::Mutex<u8> { todo() }
#[cfg(test)]
mod tests { use std::sync::Mutex; }
use lock_order::sync::{Mutex as Wrapped, RwLock};
static W: lock_order::sync::Mutex<u8> = lock_order::sync::Mutex::new(\"w\", 0);
";
    let forbid = format!("{ROWS}\n```wait-lint\nraw-locks = forbid\n```\n");
    let r = run_files(&[("src/a.rs", src)], &forbid);
    let raw: Vec<usize> = source_findings(&r).into_iter().filter(|(_, m)| m.contains("raw lock")).map(|(l, _)| l).collect();
    assert_eq!(raw, vec![1, 2, 3, 4], "{:?}", r.findings);
    let r = run_files(&[("src/a.rs", src)], ROWS);
    assert!(!source_findings(&r).iter().any(|(_, m)| m.contains("raw lock")), "allowed unless forbidden");
}

#[test]
fn a_declared_helper_awaited_inside_a_timeout_is_the_timeout_s() {
    let src = "\
async fn f() {
    let r = timeout(D, send_transaction(t)).await; // WAIT: b
    send_transaction(t).await;
    run_async(x);
}
";
    let registry = format!("{ROWS}\n```wait-lint\nwait-fns = send_transaction, run_async\n```\n");
    let r = run_files(&[("src/a.rs", src)], &registry);
    let whats: Vec<(usize, &str)> = r.sites.iter().map(|s| (s.line, s.what.as_str())).collect();
    assert_eq!(whats.len(), 3, "{whats:?}");
    assert!(whats[1].1.contains(".await") && whats[2].1.contains("run_async(..)`, a declared"), "{whats:?}");
}

#[test]
fn holding_and_branch_are_seen_through() {
    let src = "\
async fn process(n: u8) {}
async fn f() {
    lock_order::holding(\"processor\", process(1)).await;
    lock_order::branch(process(2)).await;
}
";
    let registry = "\
| Key | Kind | Waits on | Argument |
|---|---|---|---|
| `processor` | acyclic | P | x |
";
    let r = run_files(&[("src/a.rs", src)], registry);
    assert!(r.sites.is_empty(), "{:?}", r.sites);
}

#[test]
fn a_lock_class_must_be_a_registry_key() {
    let src = "\
static A: lock_order::sync::Mutex<u8> = lock_order::sync::Mutex::new(\"k\", 0);
static B: lock_order::sync::Mutex<u8> = lock_order::sync::Mutex::new(\"typo\", 0);
fn f() { let _w = lock_order::waits_on(\"nope\"); }
";
    let r = run(src);
    let bad: Vec<usize> = source_findings(&r).into_iter().filter(|(_, m)| m.contains("is no row")).map(|(l, _)| l).collect();
    assert_eq!(bad, vec![2, 3], "{:?}", r.findings);
}

#[test]
fn a_crate_is_the_path_before_its_src_component() {
    assert_eq!(crate_of("store_server/src/a.rs"), "store_server");
    assert_eq!(crate_of("tools/mysrc/x/src/b.rs"), "tools/mysrc/x");
    assert_eq!(crate_of("src/a.rs"), "");
}

#[test]
fn catch_unwind_task_local_scope_and_a_test_arm_add_no_waits() {
    let src = "\
async fn work() {}
async fn f(x: Option<u8>) {
    work().catch_unwind().await;
    TASK.scope(1, work()).await;
    match x {
        #[cfg(test)]
        Some(_) => hold().await,
        _ => {}
    }
    let mut m = std::collections::HashMap::<u8, u8>::new();
    let d: Vec<_> = m.drain().collect();
}
async fn drain() {}
";
    let r = run(src);
    assert!(r.sites.is_empty(), "{:?}", r.sites);
}

// Store no-hang §14 (phase 3b): the name collision, the empty scan, and the raw-lock escapes.

/// This code's own `async fn publish`, and a dependency client's `publish` awaited beside it.
const COLLIDES: &str = "\
struct Publisher;
impl Publisher {
    async fn publish(&self, t: &str) {}
    async fn send_status(&self) {
        self.publish(\"x\").await;
    }
}
async fn poll(client: Client) {
    client.publish(t, q, r, p).await;
}
";

#[test]
fn a_local_name_awaited_on_another_receiver_is_ambiguous_until_declared() {
    let r = run(COLLIDES);
    let ambiguous: Vec<usize> =
        source_findings(&r).into_iter().filter(|(_, m)| m.contains("receiver other than `self`")).map(|(l, _)| l).collect();
    // `self.publish` is this code's; `client.publish` may be anyone's.
    assert_eq!(ambiguous, vec![9], "{:?}", r.findings);
    assert!(r.sites.is_empty(), "an undeclared collision is no wait yet: {:?}", r.sites);
}

#[test]
fn a_declared_wait_method_wins_over_this_codes_name() {
    // The precedence the bridges rely on: rumqttc's `publish`, declared, is a wait even where
    // this code defines an `async fn publish` — its own calls included (over-tagging is safe).
    let registry = format!("{ROWS}\n```wait-lint\nwait-methods = publish\n```\n");
    let r = run_files(&[("src/a.rs", COLLIDES)], &registry);
    assert_eq!(untagged_lines(&r), vec![5, 9], "{:?}", r.findings);
    assert!(!r.findings.iter().any(|f| f.message.contains("receiver other than `self`")), "{:?}", r.findings);
}

#[test]
fn a_declared_local_method_is_this_codes_and_a_stale_one_is_a_finding() {
    let registry = format!("{ROWS}\n```wait-lint\nlocal-methods = publish\n```\n");
    let r = run_files(&[("src/a.rs", COLLIDES)], &registry);
    assert!(r.findings.iter().all(|f| !f.message.contains("receiver other than `self`")), "{:?}", r.findings);
    assert!(r.sites.is_empty(), "{:?}", r.sites);

    let stale = format!("{ROWS}\n```wait-lint\nlocal-methods = subscribe\n```\n");
    let r = run_files(&[("src/a.rs", COLLIDES)], &stale);
    assert!(registry_findings(&r).iter().any(|(_, m)| m.contains("`local-methods` names `subscribe`")), "{:?}", r.findings);
}

#[test]
fn a_collision_inside_macro_tokens_is_ambiguous_too() {
    // An `.await` inside a macro's tokens is read token by token; the receiver test is the same.
    let src = "\
impl Publisher {
    async fn publish(&self) {}
    async fn report(&self, client: Client) {
        log!(client.publish().await);
        log!(self.publish().await);
    }
}
";
    let r = run(src);
    let ambiguous: Vec<usize> =
        source_findings(&r).into_iter().filter(|(_, m)| m.contains("receiver other than `self`")).map(|(l, _)| l).collect();
    assert_eq!(ambiguous, vec![4], "{:?}", r.findings);
}

#[test]
fn the_report_counts_the_files_it_read() {
    let r = run_files(&[("src/a.rs", ONE_WAIT), ("tests/t.rs", ONE_WAIT)], ROWS);
    assert_eq!(r.files_scanned, 1, "test files are not production files");
    let r = run_files(&[], ROWS);
    assert_eq!(r.files_scanned, 0);
}

#[test]
#[should_panic(expected = "the source directories are wrong")]
fn assert_registered_refuses_a_scan_that_read_nothing() {
    let dir = std::env::temp_dir().join(format!("wait-lint-empty-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("registry.md"), format!("{ROWS}\n```wait-lint-waiters\n```\n")).unwrap();
    assert_registered(&dir, &["src"], "registry.md");
}

#[test]
fn raw_condvars_aliases_and_globs_are_raw_locks() {
    let src = "\
use std::sync::Condvar;
use std::sync as s;
use parking_lot::*;
use tokio::sync::*;
use lock_order::sync::*;
use std::sync::{Arc, mpsc};
fn f() {
    let c = std::sync::Condvar::new();
}
";
    let forbid = format!("{ROWS}\n```wait-lint\nraw-locks = forbid\n```\n");
    let r = run_files(&[("src/a.rs", src)], &forbid);
    let raw: Vec<usize> = source_findings(&r).into_iter().filter(|(_, m)| m.contains("raw lock")).map(|(l, _)| l).collect();
    assert_eq!(raw, vec![1, 2, 3, 4, 8], "{:?}", r.findings);
}

// The 3b review's findings (Claude C-14..C-18, Codex X-10).

/// Raw-lock findings, by file and line.
fn raw_lock_lines(r: &Report) -> Vec<(String, usize)> {
    r.findings.iter().filter(|f| f.message.contains("raw lock")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect()
}

fn forbidding() -> String {
    format!("{ROWS}\n```wait-lint\nraw-locks = forbid\n```\n")
}

/// Review findings C-14 / X-10: each of these named a raw lock past the check.
#[test]
fn raw_lock_escapes_through_self_aliases_extern_crates_and_bound_modules_are_found() {
    let src = "\
use std::sync::{self as s};
use parking_lot::{self as pl};
extern crate parking_lot as plx;
use tokio::sync::{self};
fn f() {
    let x = sync::Mutex::new(0);
}
";
    let r = run_files(&[("src/a.rs", src)], &forbidding());
    let lines: Vec<usize> = raw_lock_lines(&r).into_iter().map(|(_, l)| l).collect();
    assert_eq!(lines, vec![1, 2, 3, 6], "{:?}", r.findings);

    // A glob import through a name a `use` bound to a lock module.
    let glob = "use std::sync;\nuse sync::*;\n";
    let r = run_files(&[("src/a.rs", glob)], &forbidding());
    let lines: Vec<usize> = raw_lock_lines(&r).into_iter().map(|(_, l)| l).collect();
    assert_eq!(lines, vec![2], "{:?}", r.findings);
}

/// Review finding C-14: a lock module re-exported (`pub use std::sync`) and named through the
/// crate (`crate::sync::Mutex`) — and a private `use std::sync` at the root, which the whole crate
/// can name as `crate::sync` too.
#[test]
fn a_raw_lock_module_named_through_the_crate_is_found() {
    let lib = "pub use std::sync;\nmod a;\n";
    let a = "\
fn f() {
    let m = crate::sync::Mutex::new(0);
}
use crate::sync::RwLock;
use super::sync::Condvar;
";
    let r = run_files(&[("src/lib.rs", lib), ("src/a.rs", a)], &forbidding());
    assert_eq!(
        raw_lock_lines(&r),
        vec![("src/a.rs".to_string(), 2), ("src/a.rs".to_string(), 4), ("src/a.rs".to_string(), 5), ("src/lib.rs".to_string(), 1)],
        "{:?}",
        r.findings
    );
    let private_root = "use std::sync;\nmod a;\n";
    let r = run_files(&[("src/lib.rs", private_root), ("src/a.rs", a)], &forbidding());
    assert_eq!(
        raw_lock_lines(&r),
        vec![("src/a.rs".to_string(), 2), ("src/a.rs".to_string(), 4), ("src/a.rs".to_string(), 5)],
        "{:?}",
        r.findings
    );
}

/// Review finding C-14: parking_lot's other locks, its `const_*` constructors (which name no type)
/// and `lock_api`'s generic locks.
#[test]
fn every_parking_lot_and_lock_api_lock_is_a_raw_lock() {
    let src = "\
fn f() {
    let a = parking_lot::ReentrantMutex::new(0);
    let b = parking_lot::FairMutex::new(0);
    let c = parking_lot::const_mutex(0);
    let d = parking_lot::const_rwlock(0);
    let e = lock_api::Mutex::<R, u8>::new(0);
    let f = parking_lot::const_fair_mutex(0);
    let g = parking_lot::const_reentrant_mutex(0);
}
";
    let r = run_files(&[("src/a.rs", src)], &forbidding());
    let lines: Vec<usize> = raw_lock_lines(&r).into_iter().map(|(_, l)| l).collect();
    assert_eq!(lines, vec![2, 3, 4, 5, 6, 7, 8], "{:?}", r.findings);
}

/// Review finding C-15: `sync` bound to lock-order's module is the wrapper, not a raw lock; an
/// unbound `sync::Mutex` still is.
#[test]
fn the_wrappers_named_through_a_bound_module_are_not_raw_locks() {
    let src = "\
use lock_order::sync;
use lock_order::sync::{self as ls};
use sync::Condvar;
fn f() {
    let m = sync::Mutex::new(\"k\", 0);
    let n = ls::RwLock::new(\"k\", 0);
}
";
    let unbound = "fn g() {\n    let m = sync::Mutex::new(0);\n}\n";
    let r = run_files(&[("src/a.rs", src), ("src/b.rs", unbound)], &forbidding());
    assert_eq!(raw_lock_lines(&r), vec![("src/b.rs".to_string(), 2)], "{:?}", r.findings);
}

/// Review finding C-16: a bare call to a function a `use` imported from a dependency is that
/// dependency's, whatever async fn of the same name this code defines elsewhere; one that may come
/// from a dependency's glob import is a collision for the registry to resolve.
#[test]
fn an_imported_function_named_like_this_codes_is_the_import_s() {
    let imported = "\
use dep::publish;
async fn poll() {
    publish(t, q).await;
}
";
    let own = "\
pub async fn publish(t: &str) {}
async fn report() {
    publish(\"x\").await;
}
";
    let from_a_glob = "\
use dep::*;
async fn g() {
    publish(\"y\").await;
}
";
    let from_this_code = "\
use super::*;
use crate::b::publish as send_it;
async fn h() {
    publish(\"z\").await;
    send_it(\"w\").await;
}
";
    let files = [("src/a.rs", imported), ("src/b.rs", own), ("src/c.rs", from_a_glob), ("src/b/d.rs", from_this_code)];
    let r = run_files(&files, ROWS);
    let untagged: Vec<(String, usize)> =
        r.findings.iter().filter(|f| f.message.contains("untagged wait")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect();
    assert_eq!(untagged, vec![("src/a.rs".to_string(), 3)], "the imported `publish` is a wait: {:?}", r.findings);
    let collisions: Vec<(String, usize)> =
        r.findings.iter().filter(|f| f.message.contains("glob-imports")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect();
    assert_eq!(collisions, vec![("src/c.rs".to_string(), 3)], "a dependency's glob may hold this `publish`: {:?}", r.findings);

    // Declared, the collision resolves either way.
    let local = format!("{ROWS}\n```wait-lint\nlocal-methods = publish\n```\n");
    let r = run_files(&files, &local);
    assert!(!r.findings.iter().any(|f| f.message.contains("glob-imports")), "{:?}", r.findings);
    let waits = format!("{ROWS}\n```wait-lint\nwait-methods = publish\n```\n");
    let r = run_files(&files, &waits);
    assert!(!r.findings.iter().any(|f| f.message.contains("glob-imports")), "{:?}", r.findings);
    assert!(r.sites.iter().any(|s| s.file == "src/c.rs" && s.line == 3), "declared, it is a wait: {:?}", r.sites);
}

/// Review finding C-18: a scan that read files but found no wait is refused too.
#[test]
#[should_panic(expected = "the source directories are wrong")]
fn assert_registered_refuses_a_scan_that_found_no_wait() {
    let dir = std::env::temp_dir().join(format!("wait-lint-no-wait-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn f() -> u8 {\n    1\n}\n").unwrap();
    std::fs::write(dir.join("registry.md"), "| Key | Kind | Waits on | Argument |\n|---|---|---|---|\n\n```wait-lint-waiters\n```\n")
        .unwrap();
    let report = check_dirs(&dir, &["src"], "registry.md").unwrap();
    assert_eq!((report.files_scanned, report.sites.len(), report.findings.len()), (1, 0, 0), "{:?}", report.findings);
    assert_registered(&dir, &["src"], "registry.md");
}

/// A crate's own library, named by a binary of its package (`use mqtt_ynca::emulator::Emulator`
/// in `src/bin/`), is this code. mqtt_ynca's tree showed it when C-16's `use` reading first landed:
/// without the crate's name, its own library read as a dependency.
#[test]
fn a_path_through_the_crate_s_own_name_is_this_code() {
    let emulator = "pub struct Emulator;\nimpl Emulator {\n    pub async fn spawn_on(a: &str) {}\n}\n";
    let bin = "\
use mqtt_ynca::emulator::Emulator;
async fn main() {
    Emulator::spawn_on(\"x\").await;
    mqtt_ynca::emulator::Emulator::spawn_on(\"y\").await;
}
";
    let files: BTreeMap<String, String> = [("src/lib.rs", "pub mod emulator;\n"), ("src/emulator.rs", emulator), ("src/bin/emu.rs", bin)]
        .iter()
        .map(|(p, t)| (p.to_string(), t.to_string()))
        .collect();
    let own = BTreeMap::from([(String::new(), BTreeSet::from(["mqtt_ynca".to_string()]))]);
    let r = check_with_crate_names(&files, ("docs/wait-registry.md", ROWS), &own);
    assert!(r.sites.is_empty(), "{:?}", r.sites);
    let r = check(&files, ("docs/wait-registry.md", ROWS));
    let lines: Vec<usize> = r.sites.iter().map(|s| s.line).collect();
    assert_eq!(lines, vec![3, 4], "unnamed, the crate's library reads as a dependency: {:?}", r.sites);

    let manifest =
        "[package]\nname = \"mqtt-ynca\"\nversion = \"1.0.0\"\n\n[lib]\nname = \"ynca\"\n\n[dependencies]\nname = { version = \"1\" }\n";
    assert_eq!(crate_names_in(manifest), BTreeSet::from(["mqtt_ynca".to_string(), "ynca".to_string()]));
}

// The 3b re-review's findings (T2, T3).

/// `(file, line)` pairs, owned, to compare with a report's.
fn at(expected: &[(&str, usize)]) -> Vec<(String, usize)> {
    expected.iter().map(|(f, l)| (f.to_string(), *l)).collect()
}

/// Review finding T2 (a): a lock module bound at the root and rebound in a child through the crate
/// (`use crate::sync;`, `use super::sync;`), then named through the child's own binding — and a
/// grandchild's, through its parent's.
#[test]
fn a_lock_module_rebound_through_the_crate_is_found() {
    let lib = "use std::sync;\nmod a;\nmod b;\n";
    let a = "\
use crate::sync;
fn f() {
    let m = sync::Mutex::new(0);
}
mod c;
";
    let b = "\
use super::sync;
use sync::*;
fn g() {
    let c = sync::Condvar::new();
}
";
    let c = "use super::sync;\nfn h() {\n    let r = sync::RwLock::new(0);\n}\n";
    let files = [("src/lib.rs", lib), ("src/a.rs", a), ("src/b.rs", b), ("src/a/c.rs", c)];
    let r = run_files(&files, &forbidding());
    assert_eq!(
        raw_lock_lines(&r),
        at(&[("src/a.rs", 3), ("src/a/c.rs", 3), ("src/b.rs", 2), ("src/b.rs", 4)]),
        "a lock module rebound through the crate named its raw locks past the check: {:?}",
        r.findings
    );

    // The wrappers, rebound the same way, are not raw locks.
    let wrapper_root = "use lock_order::sync;\nmod a;\n";
    let wrapped = "use crate::sync;\nfn f() {\n    let m = sync::Mutex::new(\"k\", 0);\n}\n";
    let r = run_files(&[("src/lib.rs", wrapper_root), ("src/a.rs", wrapped)], &forbidding());
    assert_eq!(raw_lock_lines(&r), at(&[]), "the wrappers rebound through the crate are not raw locks: {:?}", r.findings);
}

/// Review finding T2 (b): parking_lot re-exports lock_api, so lock_api's locks are named through
/// `parking_lot::lock_api` too.
#[test]
fn lock_api_s_locks_named_through_parking_lot_are_raw_locks() {
    let src = "\
fn f() {
    let a = parking_lot::lock_api::Mutex::<parking_lot::RawMutex, u8>::new(0);
    let b: parking_lot::lock_api::RwLock<parking_lot::RawRwLock, u8> = todo();
    let c = parking_lot::lock_api::ReentrantMutex::<R, G, u8>::new(0);
}
use parking_lot::lock_api::*;
";
    let r = run_files(&[("src/a.rs", src)], &forbidding());
    let lines: Vec<usize> = raw_lock_lines(&r).into_iter().map(|(_, l)| l).collect();
    assert_eq!(lines, vec![2, 3, 4, 6], "lock_api's locks through parking_lot passed: {:?}", r.findings);
}

/// Review finding T2 (c): a glob of a module that holds a lock module (`use std::*`, `pub use
/// tokio::*`) binds `sync` without naming it, so a child's `crate::sync::Mutex` named a raw lock
/// past the check. A glob of a module holding none is not one.
#[test]
fn a_glob_of_a_module_holding_a_lock_module_is_found() {
    let lib = "\
use std::*;
pub use tokio::*;
use ::std::{*};
mod a;
use std::collections::*;
use tokio::time::*;
";
    let a = "fn f() {\n    let m = crate::sync::Mutex::new(0);\n}\n";
    let r = run_files(&[("src/lib.rs", lib), ("src/a.rs", a)], &forbidding());
    assert_eq!(
        raw_lock_lines(&r),
        at(&[("src/lib.rs", 1), ("src/lib.rs", 2), ("src/lib.rs", 3)]),
        "a glob of a module holding a lock module passed: {:?}",
        r.findings
    );
}

/// Untagged waits, by file and line.
fn untagged_at(r: &Report) -> Vec<(String, usize)> {
    r.findings.iter().filter(|f| f.message.contains("untagged wait")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect()
}

/// This code's own `async fn sleep`, which a dependency's `sleep` must not hide behind.
const OWN_SLEEP: &str = "pub async fn sleep(d: u64) {}\n";

/// Review finding T3 (a): `use super::*` carries the parent's private `use tokio::time::sleep`, so
/// the child's bare `sleep(..)` is tokio's — this code's own `async fn sleep` elsewhere hid it.
#[test]
fn a_dependency_fn_carried_by_a_glob_of_this_code_is_the_dependency_s() {
    let a = "use tokio::time::sleep;\nmod inner;\n";
    let inner = "use super::*;\nasync fn settle(d: u64) {\n    sleep(d).await;\n}\n";
    let files = [("src/lib.rs", "pub mod util;\nmod a;\n"), ("src/util.rs", OWN_SLEEP), ("src/a.rs", a), ("src/a/inner.rs", inner)];
    let r = run_files(&files, ROWS);
    assert_eq!(
        untagged_at(&r),
        at(&[("src/a/inner.rs", 3)]),
        "tokio's `sleep`, carried by `use super::*`, hid behind this code's: {:?}",
        r.findings
    );

    // The fixture's control: with no `sleep` of this code's, the call is a wait already.
    let r = run_files(&[("src/lib.rs", "mod a;\n"), ("src/a.rs", a), ("src/a/inner.rs", inner)], ROWS);
    assert_eq!(untagged_at(&r), at(&[("src/a/inner.rs", 3)]), "{:?}", r.findings);

    // A glob of a module that defines its own `sleep` carries this code's.
    let own = "pub async fn sleep(d: u64) {}\nmod inner;\n";
    let r = run_files(&[("src/lib.rs", "mod a;\n"), ("src/a.rs", own), ("src/a/inner.rs", inner)], ROWS);
    assert_eq!(untagged_at(&r), at(&[]), "the parent's own `sleep` is this code's: {:?}", r.findings);
}

/// Review finding T3 (b, c): a local module re-exports tokio's `sleep`, and it is called through
/// that module (`net::sleep`, `crate::net::sleep`, a `use` of it) — or a file calls its own import
/// as `self::sleep`.
#[test]
fn a_dependency_fn_re_exported_by_a_local_module_is_the_dependency_s() {
    let lib = "pub mod util;\nmod net;\nmod user;\nmod me;\nuse net as nn;\nasync fn f(d: u64) {\n    net::sleep(d).await;\n}\n";
    let user = "\
async fn g(d: u64) {
    crate::net::sleep(d).await;
}
use crate::net::sleep as nap;
async fn h(d: u64) {
    nap(d).await;
}
use crate::net as n;
async fn i(d: u64) {
    n::sleep(d).await;
    crate::nn::sleep(d).await;
}
";
    let me = "use tokio::time::sleep;\nasync fn k(d: u64) {\n    self::sleep(d).await;\n}\n";
    let files = |net: &'static str| {
        [("src/lib.rs", lib), ("src/util.rs", OWN_SLEEP), ("src/net.rs", net), ("src/user.rs", user), ("src/me.rs", me)]
    };
    let r = run_files(&files("pub use tokio::time::sleep;\n"), ROWS);
    assert_eq!(
        untagged_at(&r),
        at(&[("src/lib.rs", 7), ("src/me.rs", 3), ("src/user.rs", 2), ("src/user.rs", 6), ("src/user.rs", 10), ("src/user.rs", 11)]),
        "tokio's `sleep`, re-exported by a local module, hid behind this code's: {:?}",
        r.findings
    );

    // A local module re-exporting this code's own `sleep` is this code's.
    let r = run_files(&files("pub use crate::util::sleep;\n"), ROWS);
    assert_eq!(untagged_at(&r), at(&[("src/me.rs", 3)]), "only `self::sleep`, tokio's, is a wait: {:?}", r.findings);
}

/// Review finding T3's collision side: a module path into a module that neither defines nor imports
/// the name but glob-imports from a dependency (`pub use tokio::time::*`) may be the dependency's,
/// a collision for the registry; one into a module that defines it beside such a glob is the
/// module's own, since a definition shadows a glob.
#[test]
fn a_module_s_dependency_glob_may_hold_the_name() {
    let lib = "pub mod util;\nmod net;\nmod own;\nasync fn f(d: u64) {\n    net::sleep(d).await;\n    own::sleep(d).await;\n}\n";
    let net = "pub use tokio::time::*;\n";
    let own = "use tokio::time::*;\npub async fn sleep(d: u64) {}\n";
    let files = [("src/lib.rs", lib), ("src/util.rs", OWN_SLEEP), ("src/net.rs", net), ("src/own.rs", own)];
    let r = run_files(&files, ROWS);
    let collisions: Vec<(String, usize)> =
        r.findings.iter().filter(|f| f.message.contains("glob-imports")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect();
    assert_eq!(collisions, at(&[("src/lib.rs", 5)]), "`net::sleep` may be tokio's, `own::sleep` is the module's: {:?}", r.findings);
    assert_eq!(untagged_at(&r), at(&[]), "{:?}", r.findings);
}

/// Following T3's globs: modules that glob-import each other are each asked once, not once per way
/// there — sixteen of them would otherwise be 15^8 questions for one call.
#[test]
fn globs_that_reach_each_other_are_followed_once() {
    let n = 16;
    let mut files: Vec<(String, String)> = (0..n)
        .map(|i| {
            let globs: String = (0..n).filter(|k| *k != i).map(|k| format!("use crate::m{k}::*;\n")).collect();
            (format!("src/m{i}.rs"), format!("{globs}async fn f{i}() {{\n    go().await;\n}}\n"))
        })
        .collect();
    files.push(("src/util.rs".into(), "pub async fn go() {}\n".into()));
    let worker = std::thread::spawn(move || {
        let files: BTreeMap<String, String> = files.into_iter().collect();
        check(&files, ("docs/wait-registry.md", ROWS)).sites.len()
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !worker.is_finished() {
        assert!(std::time::Instant::now() < deadline, "the check of modules globbing each other did not finish within 20 s");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert_eq!(worker.join().unwrap(), 0, "`go` is this code's");
}

/// Review finding T7 (round 3): an inline module is a scope of its own. The root imports tokio's
/// `ctrl_c` and holds an unrelated inline module with its own `async fn ctrl_c`; a file child's
/// `use super::*` carries the root's import, not the inline module's function — which, read as
/// part of the root's file, hid the dependency's wait.
#[test]
fn an_inline_module_s_function_does_not_hide_its_file_s_import() {
    let lib = "use tokio::signal::ctrl_c;\nmod quiet {\n    pub async fn ctrl_c() {}\n}\nmod child;\n";
    let child = "use super::*;\nasync fn stop() {\n    ctrl_c().await;\n}\n";
    let r = run_files(&[("src/lib.rs", lib), ("src/child.rs", child)], ROWS);
    assert_eq!(
        untagged_at(&r),
        at(&[("src/child.rs", 3)]),
        "tokio's `ctrl_c`, carried by `use super::*`, hid behind an inline module's: {:?}",
        r.findings
    );

    // The fixture's control: with no inline `ctrl_c`, the call is a wait already.
    let bare = "use tokio::signal::ctrl_c;\nmod child;\n";
    let r = run_files(&[("src/lib.rs", bare), ("src/child.rs", child)], ROWS);
    assert_eq!(untagged_at(&r), at(&[("src/child.rs", 3)]), "{:?}", r.findings);

    // The root's own `ctrl_c`, at the root, is this code's.
    let own = "pub async fn ctrl_c() {}\nmod child;\n";
    let r = run_files(&[("src/lib.rs", own), ("src/child.rs", child)], ROWS);
    assert_eq!(untagged_at(&r), at(&[]), "the root's own `ctrl_c` is this code's: {:?}", r.findings);

    // At the root itself: a bare `sleep` the root glob-imports from tokio is not the inline
    // module's, and may be tokio's.
    let glob = "use tokio::time::*;\nmod quiet {\n    pub async fn sleep(d: u64) {}\n}\nasync fn f(d: u64) {\n    sleep(d).await;\n}\n";
    let r = run_files(&[("src/lib.rs", glob)], ROWS);
    let collisions: Vec<(String, usize)> =
        r.findings.iter().filter(|f| f.message.contains("glob-imports")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect();
    assert_eq!(collisions, at(&[("src/lib.rs", 6)]), "the root's bare `sleep` may be tokio's, not the inline module's: {:?}", r.findings);

    // A call written inside an inline module is followed from that module: its glob of tokio may
    // hold `sleep`, whatever `sleep` the root defines.
    let inner = "pub async fn sleep(d: u64) {}\nmod quiet {\n    use tokio::time::*;\n    async fn f(d: u64) {\n        sleep(d).await;\n    }\n}\n";
    let r = run_files(&[("src/lib.rs", inner)], ROWS);
    let collisions: Vec<(String, usize)> =
        r.findings.iter().filter(|f| f.message.contains("glob-imports")).map(|f| (f.file.clone(), f.line.unwrap_or(0))).collect();
    assert_eq!(collisions, at(&[("src/lib.rs", 5)]), "a call inside an inline module is followed from that module: {:?}", r.findings);
}

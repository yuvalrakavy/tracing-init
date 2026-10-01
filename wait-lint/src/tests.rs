use super::*;

const ROWS: &str = "\
| Key | Kind | Waits on | Held across | Argument |
|---|---|---|---|---|
| `k` | acyclic | a lock | — | nothing waits back |
| `b` | bounded | a reply | — | 5 s, then the caller fails loudly |
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
    assert_eq!(untagged_lines(&r), vec![10, 11], "the escaping `idle(e)` and `notified()`; the spawned one is its task's: {:?}", r.findings);
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

// Lock order.

fn order_registry(k_held: &str, b_held: &str) -> String {
    format!(
        "\
| Key | Kind | Waits on | Held across | Argument |
|---|---|---|---|---|
| `k` | acyclic | a lock | {k_held} | x |
| `b` | acyclic | another lock | {b_held} | y |
"
    )
}

const K_THEN_B: &str = "\
fn f(m: M, n: M) {
    let g = m.lock().unwrap(); // WAIT: k
    let h = n.lock().unwrap(); // WAIT: b
}
";

#[test]
fn a_guard_held_across_an_undeclared_wait_is_a_finding() {
    let r = run_files(&[("src/a.rs", K_THEN_B)], &order_registry("—", "—"));
    let f = source_findings(&r);
    assert!(f.iter().any(|(l, m)| *l == 3 && m.contains("a `k` guard (line 2) is held across this `b` wait")), "{f:?}");
}

#[test]
fn a_declared_edge_is_clean_and_an_unused_one_is_a_finding() {
    let r = run_files(&[("src/a.rs", K_THEN_B)], &order_registry("`b`", "—"));
    assert!(source_findings(&r).is_empty(), "{:?}", r.findings);
    let r = run_files(&[("src/a.rs", K_THEN_B)], &order_registry("`b`", "`k`"));
    assert!(registry_findings(&r).iter().any(|(_, m)| m.contains("row `b` declares `k` held across, and no `b` guard")), "{:?}", r.findings);
}

#[test]
fn a_cycle_in_the_declared_order_is_a_finding() {
    let both = "\
fn f(m: M, n: M) {
    let g = m.lock().unwrap(); // WAIT: k
    let h = n.lock().unwrap(); // WAIT: b
}
fn g(m: M, n: M) {
    let h = n.lock().unwrap(); // WAIT: b
    let g = m.lock().unwrap(); // WAIT: k
}
";
    let r = run_files(&[("src/a.rs", both)], &order_registry("`b`", "`k`"));
    let cycles: Vec<_> = registry_findings(&r).into_iter().filter(|(_, m)| m.contains("cycle")).collect();
    assert_eq!(cycles.len(), 1, "{cycles:?}");
    assert!(cycles[0].1.contains("b → k → b"), "{cycles:?}");
}

#[test]
fn taking_a_lock_again_while_holding_it_is_a_cycle() {
    let src = "\
fn f(m: M) {
    let g = m.read().unwrap(); // WAIT: k
    let h = m.read().unwrap(); // WAIT: k
}
";
    let r = run_files(&[("src/a.rs", src)], &order_registry("`k`", "—"));
    assert!(registry_findings(&r).iter().any(|(_, m)| m.contains("cycle: k → k")), "{:?}", r.findings);
}

#[test]
fn a_temporary_a_dropped_guard_and_a_closure_hold_nothing_across() {
    let src = "\
fn f(m: M, n: M) {
    let len = m.lock().unwrap().len(); // WAIT: k
    let h = n.lock().unwrap(); // WAIT: b
}
fn g(m: M, n: M) {
    let g = m.lock().unwrap(); // WAIT: k
    drop(g);
    let h = n.lock().unwrap(); // WAIT: b
}
fn h(m: M, n: M) {
    let g = m.lock().unwrap(); // WAIT: k
    let later = move || n.lock().unwrap(); // WAIT: b
}
fn i(m: M, n: M) {
    {
        let g = m.lock().unwrap(); // WAIT: k
    }
    let h = n.lock().unwrap(); // WAIT: b
}
";
    let r = run_files(&[("src/a.rs", src)], &order_registry("—", "—"));
    assert!(source_findings(&r).is_empty(), "{:?}", r.findings);
}

#[test]
fn an_awaited_guard_and_a_declared_guard_function_hold_their_rows() {
    let src = "\
async fn f(m: M, n: M) {
    let g = m.write().await; // WAIT: k
    let h = n.lock().await; // WAIT: b
}
async fn g(m: M, n: M) {
    let g = write_with_bound(m).await;
    let h = n.lock().await; // WAIT: b
}
async fn write_with_bound(m: M) -> G {
    m.write().await // WAIT: k
}
";
    let registry = format!("{}\n```wait-lint\nguard-fns = write_with_bound: k\n```\n", order_registry("`b`", "—"));
    let r = run_files(&[("src/a.rs", src)], &registry);
    assert!(source_findings(&r).is_empty(), "{:?}", r.findings);
    let r = run_files(&[("src/a.rs", src)], &format!("{}\n```wait-lint\nguard-fns = write_with_bound: k\n```\n", order_registry("—", "—")));
    let held: Vec<_> = source_findings(&r).into_iter().filter(|(_, m)| m.contains("held across")).map(|(l, _)| l).collect();
    assert_eq!(held, vec![3, 7], "{:?}", r.findings);
}

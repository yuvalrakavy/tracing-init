//! A debug process that leaves a cycle on record exits non-zero.
//!
//! A cycle is reported, never panicked on at the acquisition (a panic in a detached task is
//! swallowed, and one inside a scripting binding can abort the process mid-run). A test run
//! needs a verdict all the same, and a test harness has no teardown that runs after every test.
//! So the first cycle a process records arms an exit handler: if the record still holds a cycle
//! when the process exits, the handler prints it and ends the process with status 101 — libtest's
//! own failure status — whatever its `main` returned. A harness that provokes a cycle on purpose
//! takes it off the record ([`crate::take_cycles`]) and the exit stays clean.
//!
//! Debug builds only, like the order check, and Unix only.

/// The status a process that left a cycle on record exits with: libtest's failure status.
pub const EXIT_STATUS: i32 = 101;

/// Arm the exit handler, once per process. A no-op in this crate's own unit tests, which record
/// cycles on purpose and read them back without clearing.
pub(crate) fn arm() {
    #[cfg(all(unix, not(test)))]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            // SAFETY: `atexit` registers a plain `extern "C"` function with no captured state.
            unsafe {
                libc::atexit(at_exit);
            }
        });
    }
}

#[cfg(all(unix, not(test)))]
extern "C" fn at_exit() {
    // Other threads still run while exit handlers do, and one may hold the record's lock: never
    // wait on it here. Nor on stderr's lock — write(2) to the descriptor directly.
    let text = match crate::checker::try_cycles() {
        Some(cycles) if cycles.is_empty() => return,
        Some(cycles) => cycles.join("\n"),
        None => "(the record was busy at exit; see the `lock_order_cycle` ERRORs)".to_owned(),
    };
    let message = format!("\nlock-order: this run left lock-order cycles on record, so it fails:\n{text}\n");
    // SAFETY: a write of an owned buffer to fd 2, then `_exit`, which runs no further handlers.
    unsafe {
        libc::write(2, message.as_ptr().cast(), message.len());
        libc::_exit(EXIT_STATUS);
    }
}

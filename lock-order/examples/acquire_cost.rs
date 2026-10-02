//! The per-acquisition cost of lock-order's wrappers against the plain `std` locks they wrap
//! (Store no-hang spec §14.4: "beside a per-acquire microbenchmark").
//!
//! ```text
//! cargo run --example acquire_cost             # the debug build: the order check runs
//! cargo run --example acquire_cost --release   # release: the hooks compile away
//! ```
//!
//! Each row is the median of seven runs of `ITERS` uncontended lock-and-release cycles on one
//! thread, in nanoseconds per cycle. "held N" takes the measured lock while N other classes are
//! already held, which is what the order check scales with: every acquisition records an edge from
//! each class its context holds. Contention is not measured — a contended acquisition waits on the
//! other holder, and the watchdog's one map entry is noise beside that wait.

use std::hint::black_box;
use std::time::Instant;

const ITERS: u32 = 200_000;
const RUNS: usize = 7;

fn median_ns(mut f: impl FnMut()) -> f64 {
    let mut samples: Vec<f64> = (0..RUNS)
        .map(|_| {
            let started = Instant::now();
            for _ in 0..ITERS {
                f();
            }
            started.elapsed().as_nanos() as f64 / f64::from(ITERS)
        })
        .collect();
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[RUNS / 2]
}

/// Classes for the "held N" rows: distinct `'static` names, each its own lock.
const OUTER: [&str; 8] = ["outer-0", "outer-1", "outer-2", "outer-3", "outer-4", "outer-5", "outer-6", "outer-7"];

fn main() {
    let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
    println!("lock-order acquire cost, {profile} build, {ITERS} cycles x {RUNS} runs (median ns/cycle)");

    let plain = std::sync::Mutex::new(0u64);
    let std_ns = median_ns(|| {
        *black_box(&plain).lock().unwrap() += 1;
    });
    println!("{:<28}{std_ns:>9.1}", "std::sync::Mutex");

    let wrapped = lock_order::sync::Mutex::new("measured", 0u64);
    let wrapped_ns = median_ns(|| {
        *black_box(&wrapped).lock().unwrap() += 1;
    });
    println!("{:<28}{wrapped_ns:>9.1}   (+{:.1})", "lock_order::sync::Mutex", wrapped_ns - std_ns);

    let rw_plain = std::sync::RwLock::new(0u64);
    let rw_std_ns = median_ns(|| {
        black_box(*black_box(&rw_plain).read().unwrap());
    });
    let rw = lock_order::sync::RwLock::new("measured-rw", 0u64);
    let rw_ns = median_ns(|| {
        black_box(*black_box(&rw).read().unwrap());
    });
    println!("{:<28}{rw_std_ns:>9.1}", "std::sync::RwLock read");
    println!("{:<28}{rw_ns:>9.1}   (+{:.1})", "lock_order RwLock read", rw_ns - rw_std_ns);

    let outers: Vec<lock_order::sync::Mutex<()>> = OUTER.iter().map(|c| lock_order::sync::Mutex::new(c, ())).collect();
    for held in [1usize, 4, 8] {
        let guards: Vec<_> = outers[..held].iter().map(|m| m.lock().unwrap()).collect();
        let ns = median_ns(|| {
            *black_box(&wrapped).lock().unwrap() += 1;
        });
        drop(guards);
        println!("{:<28}{ns:>9.1}   (+{:.1})", format!("lock_order, held {held}"), ns - std_ns);
    }
}

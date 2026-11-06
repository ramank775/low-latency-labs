//! What every loom test here shares: run a model, count its executions, and
//! say how many it explored and how long it took, passed or failed. The lab
//! asks for those counts to be written down; run with `-- --nocapture` to see them.

#![allow(dead_code)] // each test file uses part of this

use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed}; // std's: outside the model, loom never sees it
use std::sync::Arc;
use std::time::Instant;

/// The bound used when `LOOM_MAX_PREEMPTIONS` is unset: the lab's rule says 2 or 3.
pub const DEFAULT_PREEMPTIONS: usize = 3;

/// A line on stderr every this many executions, for the models that take minutes.
const PROGRESS_EVERY: usize = 50_000;

/// Explore `f` under loom with `LOOM_MAX_PREEMPTIONS`, or 3 if it is unset.
pub fn explore<F>(name: &str, f: F)
where
    F: Fn() + Sync + Send + 'static,
{
    let mut b = loom::model::Builder::new();
    if b.preemption_bound.is_none() {
        b.preemption_bound = Some(DEFAULT_PREEMPTIONS);
    }
    let bound = b.preemption_bound;
    let runs = Arc::new(AtomicUsize::new(0));
    let r = runs.clone();
    let t = Instant::now();
    let label = name.to_string();
    let res = catch_unwind(AssertUnwindSafe(|| b.check(move || {
        let n = r.fetch_add(1, Relaxed) + 1;
        if n.is_multiple_of(PROGRESS_EVERY) {
            eprintln!("{label}: {n} executions so far, {:.0?}", t.elapsed()); // a long model is still moving
        }
        f()
    })));
    let n = runs.load(Relaxed);
    let bound = bound.map_or("none".to_string(), |p| p.to_string());
    match res {
        Ok(()) => println!("{name}: passed: {n} executions explored in {:.2?}, LOOM_MAX_PREEMPTIONS {bound}",
                           t.elapsed()),
        Err(e) => {
            println!("{name}: FAILED in execution {n}, after {:.2?}, LOOM_MAX_PREEMPTIONS {bound}", t.elapsed());
            resume_unwind(e)
        }
    }
}

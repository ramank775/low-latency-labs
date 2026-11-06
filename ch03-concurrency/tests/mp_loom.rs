//! Lab 3.1 under loom: both litmus patterns with two threads, checked against
//! the memory model rather than against this machine.
//!
//!     RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency --test mp_loom -- --nocapture
//!
//! The tests marked `should_panic` are the broken variants: they pass when
//! loom finds the outcome the model allows and the code did not want.

#![cfg(loom)]

mod common;

use common::explore;
use loom::sync::atomic::{fence, AtomicU64, Ordering::{self, Acquire, Relaxed, Release, SeqCst}};
use loom::sync::Arc;

// Section 3.5's test, as printed.
#[cfg(loom)]
#[test]
fn flag_publishes_data() {
    use loom::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release}};
    use loom::sync::Arc;
    loom::model(|| {
        let data = Arc::new(AtomicU64::new(0));
        let flag = Arc::new(AtomicU64::new(0));
        let (d, f) = (data.clone(), flag.clone());
        let writer = loom::thread::spawn(move || {
            d.store(42, Relaxed);
            f.store(1, Release);
        });
        if flag.load(Acquire) == 1 {
            assert_eq!(data.load(Relaxed), 42); // fails at once if both become Relaxed
        }
        writer.join().unwrap();
    });
}

/// The lab's shape, two rounds of it: the writer stores `data = k` then
/// `flag = k`; the reader loads `flag` then `data`, and a read is stale when
/// `data` is less than the `flag` it just saw.
fn message_passing(flag_store: Ordering, flag_load: Ordering) {
    let data = Arc::new(AtomicU64::new(0));
    let flag = Arc::new(AtomicU64::new(0));
    let (d, f) = (data.clone(), flag.clone());
    let writer = loom::thread::spawn(move || {
        for k in 1..=2 {
            d.store(k, Relaxed);
            f.store(k, flag_store);
        }
    });
    for _ in 0..2 {
        let fl = flag.load(flag_load);
        let da = data.load(Relaxed);
        assert!(da >= fl, "stale read: flag {fl}, data {da}");
    }
    writer.join().unwrap();
}

#[test]
fn mp_release_acquire_never_stale() {
    explore("message passing, Release/Acquire", || message_passing(Release, Acquire));
}

#[test]
#[should_panic(expected = "stale read")]
fn mp_relaxed_reads_stale() {
    explore("message passing, Relaxed (expected to fail)", || message_passing(Relaxed, Relaxed));
}

#[test]
#[should_panic(expected = "assertion")]
fn flag_relaxed_shows_data_0() {
    // Section 3.5's test with both orderings made Relaxed: "fails within the first few".
    explore("flag_publishes_data, both Relaxed (expected to fail)", || {
        let data = Arc::new(AtomicU64::new(0));
        let flag = Arc::new(AtomicU64::new(0));
        let (d, f) = (data.clone(), flag.clone());
        let writer = loom::thread::spawn(move || {
            d.store(42, Relaxed);
            f.store(1, Relaxed);
        });
        if flag.load(Relaxed) == 1 {
            assert_eq!(data.load(Relaxed), 42, "flag is 1 and data is 0");
        }
        writer.join().unwrap();
    });
}

/// One store-buffering round: A stores `x = 1` then loads `y`, B stores `y = 1`
/// then loads `x`. Both loads returning 0 is the outcome the limit check forbids.
fn store_buffering(store: Ordering, load: Ordering, fence_between: bool) {
    let x = Arc::new(AtomicU64::new(0));
    let y = Arc::new(AtomicU64::new(0));
    let (x2, y2) = (x.clone(), y.clone());
    let b = loom::thread::spawn(move || {
        y2.store(1, store);
        if fence_between { fence(SeqCst) }
        x2.load(load)
    });
    x.store(1, store);
    if fence_between { fence(SeqCst) }
    let r1 = y.load(load);
    let r2 = b.join().unwrap();
    assert!(r1 == 1 || r2 == 1, "both loads returned 0: store buffering");
}

#[test]
#[should_panic(expected = "both loads returned 0")]
fn sb_release_acquire_both_zero() {
    explore("store buffering, Release/Acquire (expected to fail)", || store_buffering(Release, Acquire, false));
}

#[test]
fn sb_fence_seqcst_never_both_zero() {
    // The lab's SeqCst variant as loom can check it: a fence(SeqCst) between each store and load.
    explore("store buffering, fence(SeqCst)", || store_buffering(Release, Acquire, true));
}

#[test]
#[should_panic(expected = "both loads returned 0")]
fn sb_seqcst_accesses_false_alarm() {
    // loom treats SeqCst loads and stores as AcqRel, so this correct code fails
    // under loom: the limitation the lab's pitfalls warn about, shown.
    explore("store buffering, SeqCst accesses (loom's false alarm)", || store_buffering(SeqCst, SeqCst, false));
}

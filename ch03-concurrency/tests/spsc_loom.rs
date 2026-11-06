//! Lab 3.4 under loom: a producer pushes 1 to K, retrying when the ring is
//! full, and a consumer pops K items and asserts they arrive as 1 to K. loom
//! also checks every slot access against every conflicting one, so a slot read
//! before its write is visible, or written while still being read, fails too.
//!
//!     RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency --test spsc_loom -- --nocapture
//!     LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency \
//!         --test spsc_loom -- --ignored --nocapture                       # N = 16, K = 18
//!
//! Broken on purpose, each expected to fail:
//!
//!     RUSTFLAGS="--cfg loom --cfg relaxed_head_store" cargo test --release -p ch03-concurrency --test spsc_loom
//!     RUSTFLAGS="--cfg loom --cfg relaxed_tail_load" cargo test --release -p ch03-concurrency --test spsc_loom

#![cfg(loom)]

mod common;

use ch03_concurrency::spsc::Ring;
use common::explore;
use loom::sync::Arc;

fn push_and_pop<const N: usize>(k: u64) {
    let ring = Arc::new(Ring::<N>::new());
    let p = ring.clone();
    let producer = loom::thread::spawn(move || {
        for v in 1..=k {
            // SAFETY: this thread is the ring's only producer.
            while unsafe { p.try_push(v) }.is_err() {
                loom::thread::yield_now(); // full: let the consumer run
            }
        }
    });
    for want in 1..=k {
        let got = loop {
            // SAFETY: the model's main thread is the ring's only consumer.
            if let Some(v) = unsafe { ring.try_pop() } { break v; }
            loom::thread::yield_now(); // empty: let the producer run
        };
        assert_eq!(got, want, "lost or duplicated: expected {want}, popped {got}");
    }
    producer.join().unwrap();
    assert!(ring.is_empty());
}

#[test]
fn ring_2_slots_3_items() {
    // K > N: full, empty and wrap-around, so both pairs are exercised.
    explore("Ring<2>, K = 3", || push_and_pop::<2>(3));
}

#[test]
#[ignore = "long: over 31 million executions and 30 minutes without finishing on a 2-vCPU VM; \
            run with LOOM_MAX_PREEMPTIONS=2 and -- --ignored"]
fn ring_16_slots_18_items() {
    explore("Ring<16>, K = 18", || push_and_pop::<16>(18));
}

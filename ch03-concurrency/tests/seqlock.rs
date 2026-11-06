//! Section 3.8's torture test for the seqlock: the writer stores [k, k, k, k]
//! for k counting up from 1, and readers assert that every read returns four
//! equal values, never older than the last. On x86 a zero proves little; the
//! loom tests and an AArch64 run are what the chapter trusts.

#![cfg(not(loom))]

use ch03_concurrency::seqlock::SeqLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};

#[test]
fn no_torn_reads() {
    let s = SeqLock::<4>::new([0; 4]);
    const K: u64 = 200_000;
    let done = AtomicBool::new(false);
    let reads = AtomicU64::new(0);
    std::thread::scope(|sc| {
        sc.spawn(|| {
            for k in 1..=K {
                s.write(&[k; 4]);
            }
            done.store(true, Release);
        });
        for _ in 0..2 {
            sc.spawn(|| {
                let (mut last, mut n) = (0, 0);
                while !done.load(Acquire) {
                    let v = s.read();
                    assert!(v.iter().all(|&x| x == v[0]), "torn read: {v:?}");
                    assert!(v[0] >= last, "went backwards: {} after {last}", v[0]);
                    last = v[0];
                    n += 1;
                }
                reads.fetch_add(n, Relaxed);
            });
        }
    });
    assert_eq!(s.read(), [K; 4]);
    assert_eq!(s.sequence(), 2 * K);
    assert!(reads.load(Relaxed) > 0);
}

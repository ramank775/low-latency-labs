//! Sections 3.4 and 3.5, and Lab 3.1: the two litmus tests.
//!
//! Message passing is the quote and its ready flag: the writer stores `data`
//! then `flag`, the reader loads `flag` then `data`. Store buffering is the two
//! strategies' limit check: each stores its own flag, then loads the other's.
//!
//! Each side of each test is one small function that is never inlined, so that
//! `cargo asm` can show the variants side by side. On x86-64 the two message-
//! passing writers are the same two `mov`s, and so are the two readers; the
//! `SeqCst` store-buffering side has an `xchg` where the `Release` side has a `mov`.
//!
//! ```text
//! cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::mp_write_relaxed
//! cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::mp_write_release
//! cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::sb_release_acquire
//! cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::sb_seqcst
//! ```

use crate::Padded;
use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release, SeqCst}};

/// The flag and its data, each on its own line: every iteration of Lab 3.1
/// moves both lines between the two cores.
#[derive(Default)]
pub struct Mp {
    pub data: Padded<AtomicU64>,
    pub flag: Padded<AtomicU64>,
}

impl Mp {
    pub fn new() -> Self { Self::default() }
}

/// `--relaxed`, writer: `data = k`, then `flag = k`, both `Relaxed`.
#[inline(never)]
pub fn mp_write_relaxed(m: &Mp, k: u64) {
    m.data.store(k, Relaxed);
    m.flag.store(k, Relaxed);
}

/// `--release-acquire`, writer: the store of `flag` is `Release`, so `data = k`
/// is visible no later than `flag = k`.
#[inline(never)]
pub fn mp_write_release(m: &Mp, k: u64) {
    m.data.store(k, Relaxed);
    m.flag.store(k, Release); // pairs with the Acquire in mp_read_acquire
}

/// `--relaxed`, reader: `(flag, data)`, loaded in that order, both `Relaxed`.
#[inline(never)]
pub fn mp_read_relaxed(m: &Mp) -> (u64, u64) {
    let f = m.flag.load(Relaxed);
    (f, m.data.load(Relaxed))
}

/// `--release-acquire`, reader: the load of `flag` is `Acquire`, so `data` is
/// at least as new as the `flag` just seen.
#[inline(never)]
pub fn mp_read_acquire(m: &Mp) -> (u64, u64) {
    let f = m.flag.load(Acquire); // pairs with the Release in mp_write_release
    (f, m.data.load(Relaxed))
}

/// The two flags of the store-buffering test, each on its own line.
#[derive(Default)]
pub struct Sb {
    pub x: Padded<AtomicU64>,
    pub y: Padded<AtomicU64>,
}

impl Sb {
    pub fn new() -> Self { Self::default() }
}

/// One side of a store-buffering round with `Release`/`Acquire`: store mine,
/// load the other's. Nothing forbids the load passing the store, and x86's
/// store buffer does exactly that.
#[inline(never)]
pub fn sb_release_acquire(mine: &AtomicU64, other: &AtomicU64) -> u64 {
    mine.store(1, Release);
    other.load(Acquire)
}

/// The same with `SeqCst`: one total order over all four accesses, so at least
/// one load sees the other thread's store. The store is an `xchg` on x86-64.
#[inline(never)]
pub fn sb_seqcst(mine: &AtomicU64, other: &AtomicU64) -> u64 {
    mine.store(1, SeqCst); // SeqCst: the check needs one total order of both flags
    other.load(SeqCst)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn message_passing_with_release_acquire_never_reads_stale() {
        let m = Mp::new();
        const N: u64 = 200_000;
        std::thread::scope(|s| {
            s.spawn(|| for k in 1..=N { mp_write_release(&m, k) });
            s.spawn(|| loop {
                let (f, d) = mp_read_acquire(&m);
                assert!(d >= f, "stale: flag {f}, data {d}");
                if f == N { break; }
            });
        });
    }

    #[test]
    fn store_buffering_sides_store_then_load() {
        let sb = Sb::new();
        assert_eq!(sb_release_acquire(&sb.x, &sb.y), 0);
        assert_eq!(sb_seqcst(&sb.y, &sb.x), 1); // one thread: program order holds
    }
}

//! Section 3.3, indivisible: the data race Rust refuses, and the order-id counter.
//!
//! The chapter's example, exactly as printed. The compiler rejects the first
//! half, so as a whole it does not build, and this doctest checks that it fails
//! with the error the comment names:
//!
//! ```compile_fail,E0499
//! use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
//!
//! let mut plain = 0u64;
//! std::thread::scope(|s| {
//!     s.spawn(|| plain += 1);
//!     s.spawn(|| plain += 1); // error[E0499]: cannot borrow `plain` as mutable more than once at a time
//! });
//!
//! let count = AtomicU64::new(0);
//! std::thread::scope(|s| {
//!     s.spawn(|| count.fetch_add(1, Relaxed));
//!     s.spawn(|| count.fetch_add(1, Relaxed));
//! });
//! assert_eq!(count.into_inner(), 2); // always
//! ```
//!
//! Its second half alone builds and runs, and never loses an increment:
//!
//! ```
//! use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
//!
//! let count = AtomicU64::new(0);
//! std::thread::scope(|s| {
//!     s.spawn(|| count.fetch_add(1, Relaxed));
//!     s.spawn(|| count.fetch_add(1, Relaxed));
//! });
//! assert_eq!(count.into_inner(), 2); // always
//! ```

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// The first version of the gateway's id counter. Each access is atomic and
/// the pair is not: two strategies woken by one market move can both load the
/// same id, and the exchange rejects the second order.
#[inline(never)]
pub fn next_id_racy(next_id: &AtomicU64) -> u64 {
    let id = next_id.load(Relaxed);
    next_id.store(id + 1, Relaxed);
    id
}

/// The fix: one read-modify-write, `lock xadd` on x86-64. Whatever the timing,
/// every call gets a different id.
#[inline(never)]
#[allow(clippy::let_and_return)] // the chapter's line, kept as printed
pub fn next_id(next_id: &AtomicU64) -> u64 {
    let id = next_id.fetch_add(1, Relaxed);
    id
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn fetch_add_hands_out_every_id_once() {
        let counter = AtomicU64::new(1);
        let (a, b) = std::thread::scope(|s| {
            let a = s.spawn(|| (0..50_000).map(|_| next_id(&counter)).collect::<Vec<_>>());
            let b = s.spawn(|| (0..50_000).map(|_| next_id(&counter)).collect::<Vec<_>>());
            (a.join().unwrap(), b.join().unwrap())
        });
        let mut all: Vec<u64> = a.into_iter().chain(b).collect();
        all.sort_unstable();
        all.dedup();
        assert_eq!(all.len(), 100_000); // no duplicate, so no rejected order
        assert_eq!(counter.load(Relaxed), 100_001);
    }

    #[test]
    fn the_racy_pair_is_correct_on_one_thread() {
        let counter = AtomicU64::new(7);
        assert_eq!(next_id_racy(&counter), 7);
        assert_eq!(next_id_racy(&counter), 8); // the bug needs a second thread
    }
}

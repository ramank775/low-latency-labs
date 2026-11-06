//! Section 3.8: one writer, many readers, only the latest. The data is itself
//! atomics read with `Relaxed`, and the orderings take two fences, in the form
//! Hans Boehm worked out for the C++ model.

use std::sync::atomic::{fence, AtomicU64, Ordering::{Acquire, Relaxed, Release}};

pub struct SeqLock<const N: usize> {
    seq: AtomicU64,
    data: [AtomicU64; N],
}

impl<const N: usize> SeqLock<N> {
    // one writer only
    pub fn write(&self, v: &[u64; N]) {
        let s = self.seq.load(Relaxed);
        self.seq.store(s + 1, Relaxed); // odd: a write is in progress
        fence(Release);                 // keeps the odd store ahead of the data stores
        for (d, x) in self.data.iter().zip(v) {
            d.store(*x, Relaxed);
        }
        self.seq.store(s + 2, Release); // even: the data is complete
    }

    pub fn read(&self) -> [u64; N] {
        loop {
            let s1 = self.seq.load(Acquire);
            if s1 & 1 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let v = std::array::from_fn(|i| self.data[i].load(Relaxed));
            fence(Acquire);             // keeps the data loads ahead of the re-check
            if self.seq.load(Relaxed) == s1 {
                return v;
            }
        }
    }
}

impl<const N: usize> SeqLock<N> {
    /// Starts even, holding `v`.
    pub fn new(v: [u64; N]) -> Self {
        SeqLock { seq: AtomicU64::new(0), data: v.map(AtomicU64::new) }
    }

    /// The counter: twice the number of completed writes, odd while one is in progress.
    pub fn sequence(&self) -> u64 { self.seq.load(Acquire) }
}

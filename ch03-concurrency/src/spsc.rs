//! Lab 3.4: `Ring<N>`, the single-producer single-consumer protocol of
//! section 3.8 and nothing more. Three steps a side:
//!
//! ```text
//!  producer, the only writer of head            consumer, the only writer of tail
//!
//!  push(m):                                     pop():
//!  1. t = tail.load(Acquire)  <-----------+     1. h = head.load(Acquire)  <----+
//!     if head - t == N: full              |        if h == tail: empty          |
//!  2. write m into slot[head % N]         |     2. read slot[tail % N]          |
//!  3. head.store(head + 1, Release) ------|-------------------------------------+
//!                                         +---- 3. tail.store(tail + 1, Release)
//! ```
//!
//! No cached copy of the other side's counter and no batching (Chapter 5's
//! ring in `ch05-structures` adds both): every push reads the `tail` line and
//! every pop reads the `head` line. `head` and `tail` sit on padded lines of
//! their own, or the benchmark would measure false sharing.
//!
//! Built on `crate::sync`, so under `RUSTFLAGS="--cfg loom"` every access is
//! loom's. Two more cfgs break the ring on purpose, for Lab 3.4's last step:
//! `--cfg relaxed_head_store` makes the producer's store of `head` `Relaxed`,
//! and `--cfg relaxed_tail_load` makes the producer's load of `tail` `Relaxed`.

use crate::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use crate::sync::cell::UnsafeCell;
use crate::Padded;
use std::mem::MaybeUninit;

pub struct Ring<const N: usize> {
    head: Padded<AtomicU64>, // items written; only the producer stores it
    tail: Padded<AtomicU64>, // items read; only the consumer stores it
    slots: [UnsafeCell<MaybeUninit<u64>>; N],
}

// SAFETY: a slot is written only by the one producer and read only by the one
// consumer, and the two head/tail pairs order those accesses; try_push and
// try_pop are unsafe so that "one producer, one consumer" is the caller's promise.
unsafe impl<const N: usize> Sync for Ring<N> {}

impl<const N: usize> Ring<N> {
    pub fn new() -> Self {
        assert!(N > 0, "a ring needs at least one slot");
        Ring {
            head: Padded(AtomicU64::new(0)),
            tail: Padded(AtomicU64::new(0)),
            slots: std::array::from_fn(|_| UnsafeCell::new(MaybeUninit::uninit())),
        }
    }

    /// Push `m`, or hand it back if the ring is full.
    ///
    /// # Safety
    /// One thread only may ever call `try_push` on this ring: it is the only
    /// writer of `head`, and of the slot it writes.
    pub unsafe fn try_push(&self, m: u64) -> Result<(), u64> {
        let head = self.head.load(Relaxed); // our own counter: only this thread stores it
        #[cfg(not(relaxed_tail_load))]
        let t = self.tail.load(Acquire); // 1. pairs with the consumer's Release on tail
        #[cfg(relaxed_tail_load)]
        let t = self.tail.load(Relaxed); //    broken on purpose: Lab 3.4
        if head - t == N as u64 {
            return Err(m); // full
        }
        // 2. write m into slot[head % N]: the consumer finished with it before storing tail > head - N
        self.slots[(head % N as u64) as usize].with_mut(|p| {
            // SAFETY: this slot is ours until head moves past it; the Acquire above saw the consumer leave it.
            unsafe { (*p).write(m) };
        });
        #[cfg(not(relaxed_head_store))]
        self.head.store(head + 1, Release); // 3. pairs with the consumer's Acquire on head
        #[cfg(relaxed_head_store)]
        self.head.store(head + 1, Relaxed); //    broken on purpose: Lab 3.4
        Ok(())
    }

    /// Pop the oldest item, or `None` if the ring is empty.
    ///
    /// # Safety
    /// One thread only may ever call `try_pop` on this ring: it is the only
    /// writer of `tail`, and the only reader of the slots.
    pub unsafe fn try_pop(&self) -> Option<u64> {
        let tail = self.tail.load(Relaxed); // our own counter: only this thread stores it
        let h = self.head.load(Acquire); // 1. pairs with the producer's Release on head
        if h == tail {
            return None; // empty
        }
        // 2. read slot[tail % N]: its write happens-before the store of head we just saw
        let m = self.slots[(tail % N as u64) as usize].with(|p| {
            // SAFETY: the producer wrote this slot before its Release of head, which our Acquire saw.
            unsafe { (*p).assume_init_read() }
        });
        self.tail.store(tail + 1, Release); // 3. pairs with the producer's Acquire on tail
        Some(m)
    }

    /// Items in the ring now, as an onlooker sees it: a snapshot that may be stale.
    pub fn len(&self) -> u64 { self.head.load(Acquire) - self.tail.load(Acquire) }

    pub fn is_empty(&self) -> bool { self.len() == 0 }
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self { Self::new() }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn fills_empties_and_wraps() {
        let r = Ring::<4>::new();
        for round in 0..10u64 {
            for i in 0..4 {
                // SAFETY: one thread is both the only producer and the only consumer.
                assert_eq!(unsafe { r.try_push(round * 4 + i) }, Ok(()));
            }
            assert_eq!(unsafe { r.try_push(99) }, Err(99)); // full
            assert_eq!(r.len(), 4);
            for i in 0..4 {
                assert_eq!(unsafe { r.try_pop() }, Some(round * 4 + i));
            }
            assert_eq!(unsafe { r.try_pop() }, None); // empty
        }
    }

    #[test]
    fn two_threads_lose_and_duplicate_nothing() {
        let r = Ring::<16>::new();
        const K: u64 = 300_000;
        std::thread::scope(|s| {
            s.spawn(|| for v in 1..=K {
                // SAFETY: this thread is the ring's only producer.
                while unsafe { r.try_push(v) }.is_err() { std::hint::spin_loop() }
            });
            s.spawn(|| for want in 1..=K {
                let got = loop {
                    // SAFETY: this thread is the ring's only consumer.
                    if let Some(v) = unsafe { r.try_pop() } { break v; }
                    std::hint::spin_loop();
                };
                assert_eq!(got, want);
            });
        });
        assert!(r.is_empty());
    }
}

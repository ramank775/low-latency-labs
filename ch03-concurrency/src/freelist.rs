//! Section 3.7: a Treiber stack of free slot indices, and the same stack with
//! a generation tag beside the index, which is the remedy for ABA.
//!
//! Built on `crate::sync`, so that `tests/freelist_loom.rs` can show the ABA of
//! the chapter's figure in the untagged list and none in the tagged one.

use crate::sync::atomic::{AtomicU32, AtomicU64, Ordering::{Acquire, Relaxed, Release}};

const NIL: u32 = u32::MAX;

pub struct FreeList {
    head: AtomicU32,
    next: Box<[AtomicU32]>, // next[i]: the free slot below slot i
}

impl FreeList {
    pub fn push(&self, i: u32) {
        let mut head = self.head.load(Relaxed);
        loop {
            self.next[i as usize].store(head, Relaxed);
            match self.head.compare_exchange_weak(head, i, Release, Relaxed) {
                Ok(_) => return,
                Err(now) => head = now, // another thread got in first: retry
            }
        }
    }

    pub fn pop(&self) -> Option<u32> {
        let mut head = self.head.load(Acquire);
        loop {
            if head == NIL { return None; }
            let below = self.next[head as usize].load(Relaxed);
            match self.head.compare_exchange_weak(head, below, Acquire, Acquire) {
                Ok(_) => return Some(head),
                Err(now) => head = now,
            }
        }
    }
}

impl FreeList {
    /// `n` slots, all free: `head -> 0 -> 1 -> ... -> n - 1`.
    pub fn new(n: u32) -> Self {
        assert!(n < NIL, "slot {NIL} is the bottom marker");
        let next = (0..n).map(|i| AtomicU32::new(if i + 1 < n { i + 1 } else { NIL })).collect();
        FreeList { head: AtomicU32::new(if n == 0 { NIL } else { 0 }), next }
    }
}

fn pack(gen: u32, idx: u32) -> u64 { (gen as u64) << 32 | idx as u64 }

/// The free list with its history in the head: a 32-bit generation above the
/// 32-bit index, incremented by every successful push and pop.
pub struct TaggedFreeList {
    head: AtomicU64,
    next: Box<[AtomicU32]>, // next[i]: the free slot below slot i
}

impl TaggedFreeList {
    pub fn pop(&self) -> Option<u32> {
        let mut head = self.head.load(Acquire);
        loop {
            let (gen, idx) = ((head >> 32) as u32, head as u32);
            if idx == NIL { return None; }
            let below = self.next[idx as usize].load(Relaxed);
            match self.head.compare_exchange_weak(head, pack(gen.wrapping_add(1), below), Acquire, Acquire) {
                Ok(_) => return Some(idx),
                Err(now) => head = now, // index or generation changed: retry
            }
        }
    }

    /// The chapter prints only `pop`; `push` bumps the generation the same way.
    pub fn push(&self, i: u32) {
        let mut head = self.head.load(Relaxed);
        loop {
            let (gen, idx) = ((head >> 32) as u32, head as u32);
            self.next[i as usize].store(idx, Relaxed);
            match self.head.compare_exchange_weak(head, pack(gen.wrapping_add(1), i), Release, Relaxed) {
                Ok(_) => return,
                Err(now) => head = now, // index or generation changed: retry
            }
        }
    }

    /// `n` slots, all free, generation 0.
    pub fn new(n: u32) -> Self {
        assert!(n < NIL, "slot {NIL} is the bottom marker");
        let next = (0..n).map(|i| AtomicU32::new(if i + 1 < n { i + 1 } else { NIL })).collect();
        TaggedFreeList { head: AtomicU64::new(pack(0, if n == 0 { NIL } else { 0 })), next }
    }

    /// The generation now: how many pushes and pops have succeeded, modulo 2^32.
    pub fn generation(&self) -> u32 { (self.head.load(Relaxed) >> 32) as u32 }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn pops_in_order_and_takes_back() {
        let f = FreeList::new(3);
        assert_eq!((f.pop(), f.pop()), (Some(0), Some(1)));
        f.push(0);
        assert_eq!((f.pop(), f.pop(), f.pop()), (Some(0), Some(2), None));
        let t = TaggedFreeList::new(3);
        assert_eq!((t.pop(), t.pop()), (Some(0), Some(1)));
        t.push(0);
        assert_eq!((t.pop(), t.pop(), t.pop()), (Some(0), Some(2), None));
        assert_eq!(t.generation(), 5); // four pops and a push succeeded; the last pop found NIL
    }

    #[test]
    fn three_threads_never_share_a_slot() {
        // Each thread pops a slot, marks it, unmarks it, and pushes it back. A slot
        // handed to two owners at once shows up as a mark that is already set.
        let f = TaggedFreeList::new(8);
        let owned: Vec<AtomicU32> = (0..8).map(|_| AtomicU32::new(0)).collect();
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| for _ in 0..50_000 {
                    if let Some(i) = f.pop() {
                        assert_eq!(owned[i as usize].swap(1, Relaxed), 0, "slot {i} has two owners");
                        owned[i as usize].store(0, Relaxed);
                        f.push(i);
                    }
                });
            }
        });
        let mut free: Vec<u32> = std::iter::from_fn(|| f.pop()).collect();
        free.sort_unstable();
        assert_eq!(free, (0..8).collect::<Vec<_>>());
    }
}

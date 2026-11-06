//! The single-producer, single-consumer ring: one writer per counter, each on
//! its own line, and a cached copy of the other side's counter so that the
//! common push or pop reads no line the other core writes.

use std::{cell::UnsafeCell, mem::MaybeUninit, sync::Arc};
use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release}};

/// One counter alone on its 64-byte line.
#[repr(align(64))]
struct Padded(AtomicU64);

pub struct Ring<T, const N: usize> {
    head: Padded,                                // items written: stored only by the producer
    tail: Padded,                                // items read: stored only by the consumer
    slots: Box<[UnsafeCell<MaybeUninit<T>>; N]>, // allocated once, never grows
}

// SAFETY: a slot is touched by one side at a time, as the counters hand it over,
// and items cross by value, so it is enough that T may be sent between threads.
unsafe impl<T: Send, const N: usize> Sync for Ring<T, N> {}

impl<T, const N: usize> Ring<T, N> {
    fn slot(&self, count: u64) -> *mut MaybeUninit<T> {
        self.slots[(count as usize) & (N - 1)].get() // a mask, not a division
    }
}

impl<T, const N: usize> Drop for Ring<T, N> {
    fn drop(&mut self) {                          // both ends are gone: drop what is left
        for c in self.tail.0.load(Relaxed)..self.head.0.load(Relaxed) {
            // SAFETY: slots from tail to head were written and not yet read.
            unsafe { (*self.slot(c)).assume_init_drop() };
        }
    }
}

pub struct Producer<T, const N: usize> { ring: Arc<Ring<T, N>>, cached_tail: u64 }
pub struct Consumer<T, const N: usize> { ring: Arc<Ring<T, N>>, cached_head: u64 }

pub fn ring<T, const N: usize>() -> (Producer<T, N>, Consumer<T, N>) {
    const { assert!(N.is_power_of_two(), "ring capacity must be a power of two") };
    let slots: Box<[_]> = (0..N).map(|_| UnsafeCell::new(MaybeUninit::uninit())).collect();
    let ring = Arc::new(Ring {
        head: Padded(AtomicU64::new(0)),
        tail: Padded(AtomicU64::new(0)),
        slots: slots.try_into().expect("exactly N slots"),
    });
    (Producer { ring: Arc::clone(&ring), cached_tail: 0 }, Consumer { ring, cached_head: 0 })
}

impl<T, const N: usize> Producer<T, N> {
    pub fn try_push(&mut self, v: T) -> Result<(), T> {
        let head = self.ring.head.0.load(Relaxed);             // our own last store
        if head - self.cached_tail == N as u64 {               // full, by our copy
            self.cached_tail = self.ring.tail.0.load(Acquire); // only now read tail's line
            if head - self.cached_tail == N as u64 {
                return Err(v);                                 // really full
            }
        }
        // SAFETY: head - tail < N, so the consumer has handed this slot back.
        unsafe { (*self.ring.slot(head)).write(v) };
        self.ring.head.0.store(head + 1, Release);             // publish one item
        Ok(())
    }

    pub fn push_batch(&mut self, items: &mut impl Iterator<Item = T>) -> usize {
        let start = self.ring.head.0.load(Relaxed);
        let (mut head, mut refreshed) = (start, false);
        loop {
            if head - self.cached_tail == N as u64 {           // full, by our copy
                if refreshed { break; }                        // looked once already
                refreshed = true;
                self.cached_tail = self.ring.tail.0.load(Acquire);
                continue;
            }
            let Some(v) = items.next() else { break };
            // SAFETY: as in try_push: head - tail < N.
            unsafe { (*self.ring.slot(head)).write(v) };
            head += 1;
        }
        if head != start {
            self.ring.head.0.store(head, Release);             // publish them all at once
        }
        (head - start) as usize
    }
}

/// Stores `tail` when dropped, so that if `drain`'s closure panics, the items
/// already handed to it are not dropped a second time by the ring.
struct StoreOnDrop<'a>(&'a AtomicU64, u64);
impl Drop for StoreOnDrop<'_> { fn drop(&mut self) { self.0.store(self.1, Release) } }

impl<T, const N: usize> Consumer<T, N> {
    pub fn try_pop(&mut self) -> Option<T> {
        let tail = self.ring.tail.0.load(Relaxed);             // our own last store
        if tail == self.cached_head {                          // empty, by our copy
            self.cached_head = self.ring.head.0.load(Acquire); // only now read head's line
            if tail == self.cached_head {
                return None;                                   // really empty
            }
        }
        // SAFETY: tail < head, and the Acquire load of head saw the write to this slot.
        let v = unsafe { (*self.ring.slot(tail)).assume_init_read() };
        self.ring.tail.0.store(tail + 1, Release);             // hand the slot back
        Some(v)
    }

    pub fn drain(&mut self, mut f: impl FnMut(T)) -> usize {
        let start = self.ring.tail.0.load(Relaxed);
        self.cached_head = self.ring.head.0.load(Acquire);     // one look at head
        if start == self.cached_head {
            return 0;                                          // nothing there: no store
        }
        let mut tail = StoreOnDrop(&self.ring.tail.0, start);
        while tail.1 != self.cached_head {
            // SAFETY: as in try_pop: below the head just loaded, each slot read once.
            let v = unsafe { (*self.ring.slot(tail.1)).assume_init_read() };
            tail.1 += 1;                                       // counted before f runs
            f(v);
        }
        (tail.1 - start) as usize                              // one Release store, on drop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn a_million_items_cross_in_order() {
        let (mut tx, mut rx) = ring::<u64, 1024>();
        let n = 1_000_000u64;
        let producer = thread::spawn(move || {
            for i in 0..n {
                let mut v = i;
                while let Err(back) = tx.try_push(v) { v = back; std::hint::spin_loop(); }
            }
        });
        let mut next = 0u64;
        while next < n {
            rx.drain(|v| { assert_eq!(v, next); next += 1; });
        }
        producer.join().unwrap();
        assert!(rx.try_pop().is_none());
    }

    #[test]
    fn full_and_empty_are_different_conditions() {
        let (mut tx, mut rx) = ring::<u32, 4>();
        for i in 0..4 { assert!(tx.try_push(i).is_ok()); }
        assert_eq!(tx.try_push(9), Err(9));                 // really full
        assert_eq!(rx.try_pop(), Some(0));
        assert!(tx.try_push(4).is_ok());                     // one slot handed back
        let mut got = Vec::new();
        assert_eq!(rx.drain(|v| got.push(v)), 4);
        assert_eq!(got, [1, 2, 3, 4]);
        assert_eq!(rx.drain(|_| unreachable!()), 0);        // empty: no store
    }

    #[test]
    fn push_batch_stops_at_full_and_publishes_once() {
        let (mut tx, mut rx) = ring::<u32, 8>();
        assert_eq!(tx.push_batch(&mut (0..20)), 8);
        assert_eq!(rx.try_pop(), Some(0));
        assert_eq!(tx.push_batch(&mut (100..120)), 1);
    }

    #[test]
    fn items_left_in_the_ring_are_dropped_once() {
        let marker = Arc::new(());
        {
            let (mut tx, _rx) = ring::<Arc<()>, 4>();
            for _ in 0..3 { assert!(tx.try_push(Arc::clone(&marker)).is_ok()); }
            assert_eq!(Arc::strong_count(&marker), 4);
        }
        assert_eq!(Arc::strong_count(&marker), 1);
    }
}

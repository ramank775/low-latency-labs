//! Section 1.5's `try_push`, inside the smallest ring that runs it: one
//! producer, one consumer, `head` and `tail` on separate padded lines, and
//! each side keeping a private copy of the other's index, so it reads the
//! other side's line only when the ring looks full (or empty).

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::Padded;

/// Slots in the ring: 1,024, as in the chapter's estimate.
pub const CAPACITY: u64 = 1024;

/// What the ring carries; a sequence number is enough to check the order.
pub type Msg = u64;

/// What both sides see: each index on its own line, then the slots.
pub struct Shared {
    pub head: Padded<AtomicU64>, // written by the producer only
    pub tail: Padded<AtomicU64>, // written by the consumer only
    slots: Box<[UnsafeCell<Msg>]>,
}

// SAFETY: a slot is written by the one producer only while `tail` says the
// consumer is done with it, and read by the one consumer only after `head`
// (stored Release, loaded Acquire) says it was written.
unsafe impl Sync for Shared {}

impl Shared {
    fn write_slot(&self, i: u64, msg: Msg) {
        // SAFETY: called only from Producer::try_push, on a slot the consumer has released.
        unsafe { *self.slots[i as usize].get() = msg }
    }

    fn read_slot(&self, i: u64) -> Msg {
        // SAFETY: called only from Consumer::try_pop, on a slot the producer has published.
        unsafe { *self.slots[i as usize].get() }
    }
}

pub struct Producer {
    head: u64,           // the next slot to write: the producer's own copy
    cached_tail: u64,    // the consumer's tail when last read
    shared: Arc<Shared>,
}

pub struct Consumer {
    tail: u64,           // the next slot to read: the consumer's own copy
    cached_head: u64,    // the producer's head when last read
    shared: Arc<Shared>,
}

/// A ring of CAPACITY slots and its two ends; neither end can be cloned.
pub fn ring() -> (Producer, Consumer) {
    let shared = Arc::new(Shared {
        head: Padded(AtomicU64::new(0)),
        tail: Padded(AtomicU64::new(0)),
        slots: (0..CAPACITY).map(|_| UnsafeCell::new(0)).collect(),
    });
    (Producer { head: 0, cached_tail: 0, shared: shared.clone() },
     Consumer { tail: 0, cached_head: 0, shared })
}

impl Producer {
    pub fn try_push(&mut self, msg: Msg) -> bool {
        if self.head - self.cached_tail == CAPACITY {
            // Ring looks full: only now read the consumer's cache line.
            self.cached_tail = self.shared.tail.load(Ordering::Acquire);
            if self.head - self.cached_tail == CAPACITY {
                return false;
            }
        }
        self.shared.write_slot(self.head % CAPACITY, msg);
        self.head += 1;
        self.shared.head.store(self.head, Ordering::Release);
        true
    }
}

impl Consumer {
    /// The mirror image: the producer's line is read only when the ring looks empty.
    pub fn try_pop(&mut self) -> Option<Msg> {
        if self.tail == self.cached_head {
            // Ring looks empty: only now read the producer's cache line.
            self.cached_head = self.shared.head.load(Ordering::Acquire);
            if self.tail == self.cached_head {
                return None;
            }
        }
        let msg = self.shared.read_slot(self.tail % CAPACITY);
        self.tail += 1;
        self.shared.tail.store(self.tail, Ordering::Release);
        Some(msg)
    }
}

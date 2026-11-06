//! The two-thread queue harness that `ring`, `pad` and `alloc` share: the same
//! producer and consumer loops around five queues of `u64`s, each behind two
//! one-method traits so that the loops are compiled once per queue and inlined.
//!
//! Two runs. Flat out, the producer pushes 0, 1, 2, ... as fast as the queue
//! takes them and the consumer checks the order and stamps the clock once per
//! block of 4,096 items: throughput, as a per-item time per block. Paced, the
//! producer spins to the next tick of its period, stamps `rdtsc` into the
//! item and pushes it, and the consumer subtracts the stamp on pop: per-item
//! latency. Flat out with stamps shows the third thing: against a full queue,
//! latency is only queueing.
//!
//! The consumer reads the counting allocator's totals when the warm-up ends
//! and again at its last item, both threads running, so `alloc` can assert
//! that nothing between allocated; the producer waits for that last read
//! before it exits, since a thread's exit frees. Every histogram is made
//! before the start.

use crate::counting::{ALLOCS, FREES};
use crate::lab::{new_hist, now, now_unfenced, pin_or_warn, Clock};
use crate::{ring, Consumer, Producer};
use hdrhistogram::Histogram;
use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;

/// Every queue's capacity: the ring's 4,096 slots, and the same bound on the others.
pub const CAP: usize = 4096;

/// Items per throughput sample.
pub const BLOCK: u64 = 4096;

/// How far the producer may run ahead of the consumer, checked once a block,
/// so that the unbounded channel cannot fill the machine's memory. Above every
/// bounded queue's capacity, so it never binds on them.
pub const WINDOW: u64 = 65_536;

/// The producer's end: false when there is no room, to be tried again.
pub trait Tx: Send { fn push(&mut self, v: u64) -> bool; }

/// The consumer's end: None when there is nothing yet.
pub trait Rx: Send { fn pop(&mut self) -> Option<u64>; }

// ---------------------------------------------------------------- the ring

impl<const N: usize> Tx for Producer<u64, N> {
    #[inline(always)]
    fn push(&mut self, v: u64) -> bool { self.try_push(v).is_ok() }
}

impl<const N: usize> Rx for Consumer<u64, N> {
    #[inline(always)]
    fn pop(&mut self) -> Option<u64> { self.try_pop() }
}

// ---------------------------------------------------------------- the standard library's channels

impl Tx for Sender<u64> {
    #[inline(always)]
    fn push(&mut self, v: u64) -> bool { self.send(v).expect("the consumer hung up"); true } // a block per 31
}

impl Tx for SyncSender<u64> {
    #[inline(always)]
    fn push(&mut self, v: u64) -> bool {
        match self.try_send(v) {                             // spin on full, as the ring's producer does
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            Err(TrySendError::Disconnected(_)) => panic!("the consumer hung up"),
        }
    }
}

impl Rx for Receiver<u64> {
    #[inline(always)]
    fn pop(&mut self) -> Option<u64> {
        self.try_recv().ok()                                 // the consumer spins on try_recv
    }
}

// ---------------------------------------------------------------- Mutex<VecDeque<u64>>

/// `Mutex<VecDeque<u64>>`, room for `CAP` reserved once and never exceeded,
/// so it never doubles; every push and every pop takes the lock.
pub fn mutex_queue() -> (MutexTx, MutexRx) {
    let q = Arc::new(Mutex::new(VecDeque::with_capacity(CAP)));
    (MutexTx(Arc::clone(&q)), MutexRx(q))
}

pub struct MutexTx(Arc<Mutex<VecDeque<u64>>>);
pub struct MutexRx(Arc<Mutex<VecDeque<u64>>>);

impl Tx for MutexTx {
    #[inline(always)]
    fn push(&mut self, v: u64) -> bool {
        let mut q = self.0.lock().expect("lock poisoned");
        if q.len() == CAP { return false; }                  // bounded like the ring
        q.push_back(v);
        true
    }
}

impl Rx for MutexRx {
    #[inline(always)]
    fn pop(&mut self) -> Option<u64> { self.0.lock().expect("lock poisoned").pop_front() }
}

// ---------------------------------------------------------------- exercise 2: one line for both counters

/// Both counters side by side on one 64-byte line: the producer's store to
/// `head` takes the line from the consumer's core and the consumer's store to
/// `tail` takes it back, on every item.
#[repr(C, align(64))]
struct OneLine {
    head: AtomicU64, // items written: stored only by the producer
    tail: AtomicU64, // items read: stored only by the consumer; 8 bytes away
}

/// The chapter's ring for `u64`s, cached copies kept, with only the padding
/// taken away: whatever changes is the false sharing.
pub struct SharedLineRing<const N: usize> {
    counters: OneLine,
    slots: Box<[UnsafeCell<u64>; N]>,
}

// SAFETY: as for Ring: the counters hand each slot to one side at a time, and a u64 is Send.
unsafe impl<const N: usize> Sync for SharedLineRing<N> {}

pub struct SharedTx<const N: usize> { ring: Arc<SharedLineRing<N>>, cached_tail: u64 }
pub struct SharedRx<const N: usize> { ring: Arc<SharedLineRing<N>>, cached_head: u64 }

pub fn shared_line_ring<const N: usize>() -> (SharedTx<N>, SharedRx<N>) {
    const { assert!(N.is_power_of_two(), "ring capacity must be a power of two") };
    let slots: Box<[_]> = (0..N).map(|_| UnsafeCell::new(0)).collect();
    let ring = Arc::new(SharedLineRing {
        counters: OneLine { head: AtomicU64::new(0), tail: AtomicU64::new(0) },
        slots: slots.try_into().expect("exactly N slots"),
    });
    (SharedTx { ring: Arc::clone(&ring), cached_tail: 0 }, SharedRx { ring, cached_head: 0 })
}

impl<const N: usize> SharedLineRing<N> {
    fn slot(&self, count: u64) -> *mut u64 { self.slots[(count as usize) & (N - 1)].get() }
}

impl<const N: usize> Tx for SharedTx<N> {
    #[inline(always)]
    fn push(&mut self, v: u64) -> bool {
        let c = &self.ring.counters;
        let head = c.head.load(Relaxed);
        if head - self.cached_tail == N as u64 {
            self.cached_tail = c.tail.load(Acquire);
            if head - self.cached_tail == N as u64 { return false; }
        }
        // SAFETY: head - tail < N, so the consumer has handed this slot back.
        unsafe { *self.ring.slot(head) = v };
        c.head.store(head + 1, Release);                     // takes the shared line from the consumer
        true
    }
}

impl<const N: usize> Rx for SharedRx<N> {
    #[inline(always)]
    fn pop(&mut self) -> Option<u64> {
        let c = &self.ring.counters;
        let tail = c.tail.load(Relaxed);
        if tail == self.cached_head {
            self.cached_head = c.head.load(Acquire);
            if tail == self.cached_head { return None; }
        }
        // SAFETY: tail < head, and the Acquire load of head saw the write to this slot.
        let v = unsafe { *self.ring.slot(tail) };
        c.tail.store(tail + 1, Release);                     // and takes it back
        Some(v)
    }
}

// ---------------------------------------------------------------- the runs

/// The five queues.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Queue { Spsc, SharedLine, Mpsc, Sync, Mutex }

impl Queue {
    pub fn name(self) -> &'static str {
        match self {
            Queue::Spsc => "spsc ring, padded",
            Queue::SharedLine => "spsc ring, one line",
            Queue::Mpsc => "mpsc::channel()",
            Queue::Sync => "mpsc::sync_channel(4096)",
            Queue::Mutex => "Mutex<VecDeque<u64>>",
        }
    }
}

/// What a run is asked to do.
#[derive(Clone, Copy, Debug)]
pub struct Cfg {
    pub items: u64,                 // items through the queue
    pub warmup: u64,                // left out of every histogram and of the allocation count
    pub cores: [Option<usize>; 2],  // producer, consumer
    pub clock: Clock,
    pub period_ns: u64,             // stamped runs: one item a period; 0: as fast as it goes
}

/// What a run measured.
pub struct Run {
    /// Flat: nanoseconds per block of `BLOCK` items. Stamped: nanoseconds from push to pop.
    pub hist: Histogram<u64>,
    pub elapsed_ns: u64,      // from the warm-up's end to the last item
    pub measured: u64,        // items after the warm-up
    pub allocs: u64,          // allocator calls between the warm-up's end and the last item
    pub frees: u64,
    pub pins: [String; 2],    // producer, consumer
    pub out_of_order: u64,    // flat: items that were not the next number; must be 0
    pub backwards: u64,       // stamped: pops that read a clock below the stamp; must be 0
}

impl Run {
    pub fn items_per_sec(&self) -> f64 { self.measured as f64 / (self.elapsed_ns.max(1) as f64 / 1e9) }
}

/// One line padded alone: the consumer's published count, read by the producer once a block.
#[repr(align(64))]
struct Padded(AtomicU64);

/// Run `q`: flat out for throughput (`stamped` false), or stamped for latency.
pub fn run(q: Queue, cfg: &Cfg, stamped: bool) -> Run {
    match q {
        Queue::Spsc => { let (tx, rx) = ring::<u64, CAP>(); go(tx, rx, cfg, stamped) }
        Queue::SharedLine => { let (tx, rx) = shared_line_ring::<CAP>(); go(tx, rx, cfg, stamped) }
        Queue::Mpsc => { let (tx, rx) = mpsc::channel::<u64>(); go(tx, rx, cfg, stamped) }
        Queue::Sync => { let (tx, rx) = mpsc::sync_channel::<u64>(CAP); go(tx, rx, cfg, stamped) }
        Queue::Mutex => { let (tx, rx) = mutex_queue(); go(tx, rx, cfg, stamped) }
    }
}

fn go<T: Tx, R: Rx>(tx: T, rx: R, cfg: &Cfg, stamped: bool) -> Run {
    if stamped { latency(tx, rx, cfg) } else { throughput(tx, rx, cfg) }
}

/// Flat out: 0, 1, 2, ... checked in order, the clock read once a block.
pub fn throughput<T: Tx, R: Rx>(mut tx: T, mut rx: R, cfg: &Cfg) -> Run {
    let (start, done, consumed) = (AtomicBool::new(false), AtomicBool::new(false), Padded(AtomicU64::new(0)));
    let (start, done, consumed) = (&start, &done, &consumed);
    let (items, warmup, clock) = (cfg.items, cfg.warmup.min(cfg.items), cfg.clock);
    thread::scope(|s| {
        let consumer = s.spawn(move || {
            let pin = pin_or_warn("consumer", cfg.cores[1]);
            let mut hist = new_hist();                            // before the count starts
            while !start.load(Acquire) { spin_loop(); }
            let (mut next, mut bad) = (0u64, 0u64);
            let (mut a0, mut f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
            let mut t_warm = now();
            let mut t_block = t_warm;
            while next < items {
                let Some(v) = rx.pop() else { spin_loop(); continue };
                bad += (v != next) as u64;
                next += 1;
                if next == warmup {                               // both threads are running
                    (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
                    t_warm = now();
                }
                if next % BLOCK == 0 {
                    let t = now();
                    if next - BLOCK >= warmup { hist.saturating_record(clock.ns(t - t_block)); }
                    t_block = t;
                    consumed.0.store(next, Relaxed);              // once a block: the producer's window
                }
            }
            let t_end = now();
            let (a1, f1) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
            done.store(true, Release);                            // only now may the producer exit
            (hist, clock.ns(t_end - t_warm), a1 - a0, f1 - f0, bad, pin)
        });
        let producer = s.spawn(move || {
            let pin = pin_or_warn("producer", cfg.cores[0]);
            while !start.load(Acquire) { spin_loop(); }
            for i in 0..items {
                if i % BLOCK == 0 {
                    while i - consumed.0.load(Relaxed) > WINDOW { spin_loop(); }
                }
                while !tx.push(i) { spin_loop(); }                // full: wait for room
            }
            while !done.load(Acquire) { spin_loop(); }            // a thread's exit frees: not inside the count
            pin
        });
        start.store(true, Release);
        let ppin = producer.join().expect("producer panicked");
        let (hist, elapsed_ns, allocs, frees, out_of_order, cpin) = consumer.join().expect("consumer panicked");
        Run { hist, elapsed_ns, measured: items - warmup, allocs, frees, pins: [ppin, cpin], out_of_order, backwards: 0 }
    })
}

/// Stamped: the producer spins to each tick of the period (none when it is 0),
/// reads the clock and pushes the reading; the consumer subtracts it on pop.
/// A push that finds no room is retried with a fresh stamp, so the number is
/// the time the item spent in the queue.
pub fn latency<T: Tx, R: Rx>(mut tx: T, mut rx: R, cfg: &Cfg) -> Run {
    let (start, done, consumed) = (AtomicBool::new(false), AtomicBool::new(false), Padded(AtomicU64::new(0)));
    let (start, done, consumed) = (&start, &done, &consumed);
    let (items, warmup, clock) = (cfg.items, cfg.warmup.min(cfg.items), cfg.clock);
    let period = clock.ticks(cfg.period_ns);
    thread::scope(|s| {
        let consumer = s.spawn(move || {
            let pin = pin_or_warn("consumer", cfg.cores[1]);
            let mut hist = new_hist();
            while !start.load(Acquire) { spin_loop(); }
            let (mut k, mut back) = (0u64, 0u64);
            let (mut a0, mut f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
            let mut t_warm = now();
            while k < items {
                let Some(stamp) = rx.pop() else { spin_loop(); continue };
                let t = now();
                k += 1;
                if t < stamp { back += 1; }
                if k > warmup { hist.saturating_record(clock.ns(t.saturating_sub(stamp))); }
                if k == warmup { (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed)); t_warm = now(); }
                if k % BLOCK == 0 { consumed.0.store(k, Relaxed); }
            }
            let t_end = now();
            let (a1, f1) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
            done.store(true, Release);                            // only now may the producer exit
            (hist, clock.ns(t_end - t_warm), a1 - a0, f1 - f0, back, pin)
        });
        let producer = s.spawn(move || {
            let pin = pin_or_warn("producer", cfg.cores[0]);
            while !start.load(Acquire) { spin_loop(); }
            let mut tick = now() + period;
            for i in 0..items {
                if period > 0 {
                    while now_unfenced() < tick { spin_loop(); }  // open loop: the schedule, not the queue, sets the pace
                    tick += period;
                } else if i % BLOCK == 0 {
                    while i - consumed.0.load(Relaxed) > WINDOW { spin_loop(); }
                }
                while !tx.push(now()) { spin_loop(); }            // a fresh stamp each try
            }
            while !done.load(Acquire) { spin_loop(); }
            pin
        });
        start.store(true, Release);
        let ppin = producer.join().expect("producer panicked");
        let (hist, elapsed_ns, allocs, frees, backwards, cpin) = consumer.join().expect("consumer panicked");
        Run { hist, elapsed_ns, measured: items - warmup, allocs, frees, pins: [ppin, cpin], out_of_order: 0, backwards }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(items: u64, period_ns: u64) -> Cfg {
        Cfg { items, warmup: items / 4, cores: [None, None], clock: Clock { ticks_per_ns: 1.0, pair_ticks: 0 }, period_ns }
    }

    #[test]
    fn every_queue_delivers_every_item_in_order() {
        for q in [Queue::Spsc, Queue::SharedLine, Queue::Mpsc, Queue::Sync, Queue::Mutex] {
            let r = run(q, &cfg(200_000, 0), false);
            assert_eq!(r.out_of_order, 0, "{q:?}");
            assert_eq!(r.measured, 150_000, "{q:?}");
            assert!(r.hist.len() >= 30, "{q:?}: {} blocks", r.hist.len());
        }
    }

    #[test]
    fn stamped_runs_count_every_item_after_the_warm_up() {
        for q in [Queue::Spsc, Queue::Mutex] {
            let r = run(q, &cfg(20_000, 0), true);
            assert_eq!((r.hist.len(), r.backwards), (15_000, 0), "{q:?}");
            let r = run(q, &cfg(2_000, 1_000), true);          // paced at 1 µs
            assert_eq!(r.hist.len(), 1_500, "{q:?}");
        }
    }

    #[test]
    fn the_shared_line_ring_is_full_and_empty_like_the_ring() {
        let (mut tx, mut rx) = shared_line_ring::<4>();
        for i in 0..4 { assert!(tx.push(i)); }
        assert!(!tx.push(9));
        assert_eq!(rx.pop(), Some(0));
        assert!(tx.push(4));
        assert_eq!((rx.pop(), rx.pop(), rx.pop(), rx.pop(), rx.pop()), (Some(1), Some(2), Some(3), Some(4), None));
        assert_eq!(std::mem::offset_of!(OneLine, tail), 8);     // the two counters 8 bytes apart, one line
    }
}

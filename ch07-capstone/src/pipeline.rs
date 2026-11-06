//! The three threads and how they are put together.
//!
//! ```text
//!  core 2: feed thread             core 3: strategy thread           core 6: gateway stub
//!  next record from the file       drain(|s| ...)              t2    drain(|o| ...)    t4
//!  spin to its arrival tick   t0     handler.on_update(s.u)          count it
//!  decode 48 B -> Update      t1     once per batch, if the top moved
//!  try_push(Stamped) ----->          and handler.state is Live: on_top
//!                                  try_push(StampedOrder)     t3 ----->
//! ```
//!
//! The main thread is not pinned: it maps the file and makes the rings before
//! the run, waits while it runs, and collects what the threads hand back.

use crate::queue::{self, Queue, QueueRx, QueueTx};
use crate::stats::{gettid, ThreadStats};
use crate::{decode, interval_ns, new_hist, now, now_cpu, now_unfenced, record, stage};
use crate::{Batch, Hists, Log, NewBook, Replay, Report, Run, Stamped, StampedOrder, Ticks, BATCH};
use ch05_structures::counting::{self, ALLOCS, FREES};
use ch05_structures::{ring, Consumer, Producer};
use ch06_book::{ArrayBook, BookState, FeedHandler, Quoter, TreeBook, VecBook, LEVELS};
use core_affinity::CoreId;
use hdrhistogram::Histogram;
use std::hint::spin_loop;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// `--book array|tree|vec`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BookKind { Array, Tree, Vec }

/// What `run` was asked to do.
#[derive(Clone, Debug)]
pub struct Config {
    pub paced: bool,                   // --paced (true) or --flat
    pub book: BookKind,
    pub queue: Queue,
    pub records: usize,                // at most this many of the file's records
    pub warmup: u64,                   // messages run through every line and left out of the numbers
    pub stall_every_ms: u64,           // 0: off
    pub feed_core: Option<usize>,      // None: unpinned
    pub strategy_core: Option<usize>,
    pub gateway_core: Option<usize>,
    pub tpn: f64,                      // TSC ticks per ns, from calibrate()
}

/// Time from making the threads to the first release: long enough for each
/// to pin itself and fault in its book, histograms and buffers.
const LEAD_MS: u64 = 200;

// ---------------------------------------------------------------- core 3: the strategy

pub fn strategy<B: NewBook, Q: QueueRx>(mut rx: Q,           // data-structures chapter: in, from core 2
            mut tx: Producer<StampedOrder, 1024>,            // data-structures chapter: out, to core 6
            mut run: Run) -> Report {                        // this chapter: what main prepared
    if let Some(id) = run.core { assert!(core_affinity::set_for_current(CoreId { id })); } // OS chapter: pin, then allocate
    let mut handler = FeedHandler { book: B::new(run.base),  // book chapter: 65,536 levels a side
        state: BookState::Live, next_seq: run.first_seq,      // book chapter: the file starts in order
        buffer: Vec::with_capacity(4096) };                  // book chapter: the gap buffer, reserved now
    let mut h = Hists::new();                                // this chapter: histograms, worst 100
    let mut b = Batch::boxed();                              // this chapter: stamps for 4,096 messages
    loop {                                                   // threads chapter: spin, never sleep
        run.stall();                                         // this chapter: --stall-every, off unless given
        let fin = run.done.load(Acquire);                    // threads chapter: the flag, then the ring
        let (mut n, mut top) = (0, None);                    // data-structures chapter: a new batch
        rx.drain(|s| {                                       // data-structures chapter: all, publish once
            #[cfg(feature = "stages")] { b.t2[n] = now(); b.t1[n] = s.t1; } // this chapter: t2
            top = handler.on_update(s.u).or(top);            // book chapter: check, apply, compare
            b.t0[n] = s.t0; b.seq[n] = s.u.seq; n += 1;      // this chapter: a store, not a record
        });
        if n == 0 && fin { return Report { h, state: handler.state, log: run.log }; } // threads chapter: done
        if n == 0 { spin_loop(); continue; }                 // threads chapter: pause, look again
        if !matches!(handler.state, BookState::Live) { top = None; } // book chapter: quote only if Live
        let orders = top.map_or([None, None], |t| run.quoter.on_top(t)); // book chapter: one quote
        let t3 = now();                                      // this chapter: the decision, once a batch
        for o in orders.into_iter().flatten() {              // book chapter: zero, one or two orders
            if tx.try_push(StampedOrder { t3, o }).is_err() { h.dropped += 1; } // data-structures chapter
        }
        run.log.push(b.seq[n - 1], handler.book.top(), run.quoter.resting); // this chapter: decisions file
        for i in 0..n {                                      // this chapter: records, after t3
            if b.seq[i] < run.warm_seq { continue; }         // this chapter: warm-up stays out
            let total = run.ns(t3, b.t0[i]);                 // this chapter: wire to decision, ns
            h.total.saturating_record(total);                // this chapter: clamps, never allocates
            h.worst.offer(total, b.seq[i]);                  // this chapter: keep the slow ones' seqs
            #[cfg(feature = "stages")] {                     // this chapter: the breakdown build only
                h.decode.saturating_record(run.ns(b.t1[i], b.t0[i]), b.seq[i]);  // this chapter: t1 − t0
                h.hop.saturating_record(run.ns(b.t2[i], b.t1[i]), b.seq[i]);     // this chapter: t2 − t1
                h.think.saturating_record(run.ns(t3, b.t2[i]), b.seq[i]);        // this chapter: t3 − t2
            }
        }
    }
}

// ---------------------------------------------------------------- core 2: the feed

/// What the feed thread needs: the file, the schedule and its flags.
pub struct Feed<'a> {
    pub replay: &'a Replay,
    pub records: usize,
    pub paced: bool,
    pub start: u64,              // the tick the run began: record 0 is due then
    pub tpn: f64,
    pub warm_seq: u64,
    pub core: Option<usize>,
    pub done: Arc<AtomicBool>,   // stored after the last push
    pub warm: Arc<AtomicBool>,   // stored once warm_seq is pushed
}

#[derive(Default, Debug, Clone, Copy)]
pub struct FeedReport {
    pub pushed: u64,
    pub bad: u64,                // records that did not decode: must stay 0
    pub full: u64,               // times the queue was full and the feed waited
    pub late: u64,               // paced: messages released more than 1 µs after their tick
}

pub fn feed<Q: QueueTx>(mut tx: Q, f: Feed<'_>) -> FeedReport {
    if let Some(id) = f.core { assert!(core_affinity::set_for_current(CoreId { id })); } // pin first
    let late = (1_000.0 * f.tpn) as u64;                     // 1 µs in ticks
    let mut r = FeedReport::default();
    while now_unfenced() < f.start { spin_loop(); }          // the others are pinned and allocated by now
    for rec in f.replay.records().take(f.records) {
        let t0 = if f.paced {
            let due = f.start + (rec.t_arrival_ns as f64 * f.tpn) as u64;
            let mut t = now_unfenced();
            while t < due { spin_loop(); t = now_unfenced(); } // spin to the arrival tick
            if t - due > late { r.late += 1; }
            due                                              // open loop: the schedule, not the spin's end
        } else {
            now()                                            // flat: just before the decode
        };
        let Some(u) = decode(rec.msg) else { r.bad += 1; continue };
        let mut s = Stamped { t0, t1: stage(), u };          // t1: decode done, stages build only
        while let Err(back) = tx.try_push(s) {               // full: wait, never drop an update
            s = back;
            r.full += 1;
            spin_loop();
        }
        r.pushed += 1;
        if u.seq == f.warm_seq { f.warm.store(true, Release); }
    }
    f.done.store(true, Release);                             // after the last push, as the strategy expects
    r
}

// ---------------------------------------------------------------- core 6: the gateway stub

pub struct Gateway {
    pub tpn: f64,
    pub core: Option<usize>,
    pub done: Arc<AtomicBool>,   // the strategy has returned
    pub warm: Arc<AtomicBool>,   // the warm-up is over: start recording
    pub backwards: Arc<AtomicU64>, // the strategy's counter: a t4 below its t3 counts there too
}

pub struct GatewayReport {
    pub out_hop: Histogram<u64>, // t4 - t3, the order ring from core 3 to core 6
    pub orders: u64,
}

/// Stands in for the thread that owns the order connection: drains the output
/// ring, stamps each order and counts it, and sends nothing.
pub fn gateway(mut rx: Consumer<StampedOrder, 1024>, g: Gateway) -> GatewayReport {
    if let Some(id) = g.core { assert!(core_affinity::set_for_current(CoreId { id })); } // pin first
    let mut out_hop = new_hist();                            // core 6 owns t4, so it owns this
    let mut orders = 0u64;
    let ns_per_tick = 1.0 / g.tpn;
    loop {
        let fin = g.done.load(Acquire);                      // the flag, then the ring
        let warm = g.warm.load(Relaxed);
        let n = rx.drain(|so| {
            let t4 = now();                                  // the order has left the ring
            orders += 1;
            if warm { record(&mut out_hop, interval_ns(t4, so.t3, ns_per_tick, &g.backwards)); } // after its own read
        });
        if n == 0 && fin { break; }
        if n == 0 { spin_loop(); }
    }
    GatewayReport { out_hop, orders }
}

// ---------------------------------------------------------------- assembling a run

/// One thread's kernel counts at the end of warm-up and at the end of the run.
#[derive(Clone, Copy, Debug)]
pub struct ThreadInfo {
    pub name: &'static str,
    pub core: Option<usize>,
    pub warm: Option<ThreadStats>,
    pub end: Option<ThreadStats>,
    pub cpu_end: u32,            // rdtscp's core number after the loop
}

impl ThreadInfo {
    /// The counts after warm-up, when both ends were read.
    pub fn after_warmup(&self) -> Option<ThreadStats> {
        Some(self.end?.since(&self.warm?))
    }
}

pub struct Summary {
    pub report: Report,
    pub out_hop: Histogram<u64>,
    pub orders: u64,             // counted by the gateway
    pub feed: FeedReport,
    pub threads: [ThreadInfo; 3],
    pub allocs: Option<(u64, u64)>, // (allocations, frees) after warm-up; None without the counting allocator
    pub records: usize,
    pub first_seq: u64,
    pub warm_seq: u64,
    pub base: Ticks,
    pub elapsed_ns: u64,         // first release to the strategy's return
    pub backwards: u64,          // intervals that came out negative, strategy and gateway: must be 0
}

/// The quoter every run uses: 2 ticks either side of the mid, 100 lots. With
/// no fills the inventory stays 0, so the quote it wants is a function of the top alone.
pub fn quoter() -> Quoter {
    Quoter { inventory: 0, half_spread: 2, skew_per_lot: 1, size: 100, resting: [None, None] }
}

/// Replay `r` once with `cfg`'s pieces: the book and queue are type
/// parameters, chosen here, so the loops above are the same code for each swap.
pub fn replay(r: &Replay, cfg: &Config) -> Summary {
    match cfg.book {
        BookKind::Array => with_queue::<ArrayBook>(r, cfg),
        BookKind::Tree => with_queue::<TreeBook>(r, cfg),
        BookKind::Vec => with_queue::<VecBook>(r, cfg),
    }
}

fn with_queue<B: NewBook>(r: &Replay, cfg: &Config) -> Summary {
    match cfg.queue {
        Queue::Spsc => { let (tx, rx) = ring::<Stamped, BATCH>(); go::<B, _, _>(r, cfg, tx, rx) }
        Queue::Mutex => { let (tx, rx) = queue::mutex(); go::<B, _, _>(r, cfg, tx, rx) }
        Queue::Mpsc => { let (tx, rx) = std::sync::mpsc::channel::<Stamped>(); go::<B, _, _>(r, cfg, tx, rx) }
    }
}

/// Wait at the end of a thread until main has taken the warm-up counts, then
/// take this thread's own: /proc forgets a thread once it exits.
fn finish(snapped: &AtomicBool) -> (Option<ThreadStats>, u32) {
    while !snapped.load(Acquire) { thread::sleep(Duration::from_millis(1)); }
    (ThreadStats::own(), now_cpu().1)
}

fn go<B: NewBook, T: QueueTx, R: QueueRx>(r: &Replay, cfg: &Config, tx: T, rx: R) -> Summary {
    let records = cfg.records.min(r.len());
    let first_seq = r.first_seq().unwrap_or(1);
    let base = r.first_mid().unwrap_or(LEVELS as Ticks / 2) - LEVELS as Ticks / 2; // window centred on it
    let warm_seq = first_seq + cfg.warmup;
    let ticks = |ms: u64| (ms as f64 * 1e6 * cfg.tpn) as u64;
    let counting = counting::installed();

    let done = Arc::new(AtomicBool::new(false));     // feed -> strategy
    let warm = Arc::new(AtomicBool::new(false));     // feed -> main and gateway
    let sdone = Arc::new(AtomicBool::new(false));    // strategy -> gateway and main
    let snapped = AtomicBool::new(false);            // main -> all: the warm-up counts are taken
    let tids = [AtomicI32::new(0), AtomicI32::new(0), AtomicI32::new(0)];
    let backwards = Arc::new(AtomicU64::new(0));    // strategy and gateway -> main, read after the joins
    let (otx, orx) = ring::<StampedOrder, 1024>();

    let start = now() + ticks(LEAD_MS);
    let run = Run {
        base, quoter: quoter(), ns_per_tick: 1.0 / cfg.tpn, first_seq, warm_seq,
        log: Log::with_capacity(records), done: Arc::clone(&done), core: cfg.strategy_core,
        stall_every: ticks(cfg.stall_every_ms), stall_len: ticks(10),
        next_stall: start + ticks(cfg.stall_every_ms),         // the first, one interval into the run
        backwards: Arc::clone(&backwards),
    };
    let f = Feed { replay: r, records, paced: cfg.paced, start, tpn: cfg.tpn, warm_seq,
                   core: cfg.feed_core, done: Arc::clone(&done), warm: Arc::clone(&warm) };
    let g = Gateway { tpn: cfg.tpn, core: cfg.gateway_core, done: Arc::clone(&sdone), warm: Arc::clone(&warm),
                      backwards: Arc::clone(&backwards) };

    thread::scope(|sc| {
        let (tids, snapped, sdone_s) = (&tids, &snapped, Arc::clone(&sdone));
        let feed_t = thread::Builder::new().name("feed".into()).spawn_scoped(sc, move || {
            tids[0].store(gettid(), Release);
            let rep = feed(tx, f);
            let (st, cpu) = finish(snapped);
            (rep, st, cpu)
        }).expect("spawn feed");
        let strat_t = thread::Builder::new().name("strategy".into()).spawn_scoped(sc, move || {
            tids[1].store(gettid(), Release);
            let rep = strategy::<B, R>(rx, otx, run);
            let end = now();
            sdone_s.store(true, Release);                     // the gateway may stop once drained
            let (st, cpu) = finish(snapped);
            (rep, end, st, cpu)
        }).expect("spawn strategy");
        let gate_t = thread::Builder::new().name("gateway".into()).spawn_scoped(sc, move || {
            tids[2].store(gettid(), Release);
            let rep = gateway(orx, g);
            let (st, cpu) = finish(snapped);
            (rep, st, cpu)
        }).expect("spawn gateway");

        // main: off the hot cores, wakes once a millisecond until warm-up ends
        while !warm.load(Acquire) && !sdone.load(Acquire) { thread::sleep(Duration::from_millis(1)); }
        let at_warm = [0, 1, 2].map(|i| ThreadStats::of(tids[i].load(Acquire)));
        let (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
        let warmed = warm.load(Acquire);
        snapped.store(true, Release);

        let (feed_rep, feed_st, feed_cpu) = feed_t.join().expect("feed thread");
        let (report, end, strat_st, strat_cpu) = strat_t.join().expect("strategy thread");
        let (gate_rep, gate_st, gate_cpu) = gate_t.join().expect("gateway thread");
        let (a1, f1) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));

        let info = |i: usize, name, core, end, cpu_end| ThreadInfo { name, core, warm: at_warm[i], end, cpu_end };
        Summary {
            report,
            out_hop: gate_rep.out_hop,
            orders: gate_rep.orders,
            feed: feed_rep,
            threads: [info(0, "feed", cfg.feed_core, feed_st, feed_cpu),
                      info(1, "strategy", cfg.strategy_core, strat_st, strat_cpu),
                      info(2, "gateway", cfg.gateway_core, gate_st, gate_cpu)],
            allocs: (counting && warmed).then_some((a1 - a0, f1 - f0)),
            records, first_seq, warm_seq, base,
            elapsed_ns: ((end - start) as f64 / cfg.tpn) as u64,
            backwards: backwards.load(Relaxed),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::synth;

    fn cfg(book: BookKind, queue: Queue) -> Config {
        Config { paced: false, book, queue, records: usize::MAX, warmup: 5_000, stall_every_ms: 0,
                 feed_core: None, strategy_core: None, gateway_core: None, tpn: crate::calibrate() }
    }

    #[test]
    fn a_flat_run_applies_every_update_and_stays_live() {
        let r = Replay::from_bytes(synth::generate(&synth::Params { records: 30_000, ..Default::default() }));
        let s = replay(&r, &cfg(BookKind::Array, Queue::Spsc));
        assert_eq!(s.report.state, BookState::Live);
        assert_eq!(s.feed.pushed, 30_000);
        assert_eq!(s.report.h.total.len(), 25_000);
        assert_eq!(s.report.h.dropped, 0);
        assert_eq!(s.report.log.full, 0);
        assert_eq!(s.report.log.v.last().map(|d| d.seq), Some(30_000));
        let sent = s.report.log.write_orders(std::io::sink()).unwrap();
        assert_eq!(sent, s.orders, "the gateway counted every order the log says was sent");
    }

    /// Holds one push for 20 ms, as a full ring behind a stalled strategy would.
    struct Stalling { got: Vec<Stamped>, stall_at: usize, stall: u64 }

    impl QueueTx for &mut Stalling {
        fn try_push(&mut self, s: Stamped) -> Result<(), Stamped> {
            if self.got.len() == self.stall_at { let t = now(); while now() - t < self.stall { spin_loop(); } }
            self.got.push(s);
            Ok(())
        }
    }

    /// The open-loop rule: in paced mode t0 is the scheduled tick, even for the
    /// messages released late after the feed was held up.
    #[test]
    fn paced_t0_is_the_scheduled_tick_even_after_a_stall() {
        let tpn = crate::calibrate();
        let r = Replay::from_bytes(synth::generate(&synth::Params { records: 20_000, ..Default::default() }));
        let mut q = Stalling { got: Vec::with_capacity(20_000), stall_at: 5_000, stall: (20e6 * tpn) as u64 };
        let start = now() + (10e6 * tpn) as u64;
        let f = Feed { replay: &r, records: usize::MAX, paced: true, start, tpn, warm_seq: 1, core: None,
                       done: Arc::new(AtomicBool::new(false)), warm: Arc::new(AtomicBool::new(false)) };
        let rep = feed(&mut q, f);
        assert!(rep.late > 1_000, "a 20 ms stall made only {} messages late", rep.late);
        for (k, s) in q.got.iter().enumerate() {
            assert_eq!(s.t0, start + (r.record(k).t_arrival_ns as f64 * tpn) as u64, "message {k}: t0 is not its tick");
        }
    }
}

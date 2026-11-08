//! The capstone's library: the clock, the histograms and the worst 100, the
//! types that cross between threads, and the replay reader. The three thread
//! loops are in `pipeline`, the queue swaps in `queue`, the synthetic feed in
//! `synth`, the kernel's per-thread counters in `stats`.

use core::arch::x86_64::{__rdtscp, _mm_lfence, _rdtsc};
use std::hint::spin_loop;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

use hdrhistogram::Histogram;

pub mod pipeline;
pub mod queue;
pub mod stats;
pub mod synth;

pub use ch05_structures::{Side, Update};
pub use ch06_book::{ArrayBook, Book, BookState, Lots, Order, Quoter, Ticks, Top, TreeBook, VecBook, LEVELS};

// ---------------------------------------------------------------- the clock

/// The TSC, read once every earlier instruction has finished.
#[inline(always)]
pub fn now() -> u64 {
    // SAFETY: lfence and rdtsc exist on every x86_64 part and touch no memory.
    unsafe {
        _mm_lfence();   // wait for earlier instructions: do not read early
        _rdtsc()
    }
}

/// The TSC and the number of the core that read it.
#[inline(always)]
pub fn now_cpu() -> (u64, u32) {
    let mut aux = 0u32;
    // SAFETY: rdtscp (the `rdtscp` flag in /proc/cpuinfo) writes only `aux`, a live local.
    let t = unsafe { __rdtscp(&mut aux) };  // aux: TSC_AUX, filled by the kernel at boot
    (t, aux & 0xfff)                        // low 12 bits: the CPU; above them, the node
}

/// The TSC with no fence: for spinning until a tick, where a read taken a
/// little early costs one more turn of the loop and nothing else.
#[inline(always)]
pub fn now_unfenced() -> u64 {
    // SAFETY: as in now(): rdtsc exists on every x86_64 part and touches no memory.
    unsafe { _rdtsc() }
}

/// TSC ticks per nanosecond, measured once at startup against
/// CLOCK_MONOTONIC_RAW, the kernel clock the time daemon never slews.
pub fn calibrate() -> f64 {
    let raw = || {  // CLOCK_MONOTONIC_RAW, in ns
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime only writes ts, a live local.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    };
    let (n0, t0) = (raw(), now());
    while raw() - n0 < 200_000_000 {}  // spin for 200 ms
    let (n1, t1) = (raw(), now());
    (t1 - t0) as f64 / (n1 - n0) as f64
}

/// The p50 of `pairs` back-to-back `now()` pairs, in ticks: the clock's own
/// share of every interval it closes, to subtract from each stage.
pub fn clock_pair_p50(pairs: usize) -> u64 {
    let mut h = new_hist();
    for _ in 0..pairs {
        let a = now();
        let b = now();
        h.saturating_record(b - a);                     // ticks, not ns
    }
    h.value_at_quantile(0.50)
}

/// A stage stamp: a fenced read in a build made with `--features stages`;
/// in any other build, a constant the compiler folds away.
#[inline(always)]
pub fn stage() -> u64 {
    if cfg!(feature = "stages") { now() } else { 0 }
}

// ---------------------------------------------------------------- histograms

/// Before the run: every byte the histogram will ever use.
pub fn new_hist() -> Histogram<u64> {
    // 1 ns to 1 s at 3 significant figures: 21,504 counters, about 170 KB
    Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).unwrap()
}

/// `later − earlier` in nanoseconds, after the timed region: a compare, a
/// subtract and one multiply. A later stamp below an earlier one can only mean
/// two cores' counters disagree: it adds one to `backwards` and gives 0 ns,
/// where the wrapped `u64` would have been clamped to a false 1 s.
#[inline(always)]
pub fn interval_ns(later: u64, earlier: u64, ns_per_tick: f64, backwards: &AtomicU64) -> u64 {
    if later < earlier { backwards.fetch_add(1, Relaxed); return 0; }   // never, on one synchronised counter
    ((later - earlier) as f64 * ns_per_tick) as u64
}

/// In the loop, after the timed region: a few branches, a leading_zeros, one add.
#[inline]
pub fn record(h: &mut Histogram<u64>, ns: u64) {
    h.saturating_record(ns);      // above 1 s counts as 1 s: never lost, never allocates
}

/// After the run, on the main thread: per-thread histograms added counter by counter.
pub fn merge(per_thread: &[Histogram<u64>]) -> Histogram<u64> {
    let mut all = new_hist();
    for h in per_thread {
        all.add(h).unwrap();      // same bounds, so the sum is exact
    }
    all
}

pub fn report(name: &str, h: &Histogram<u64>) {
    println!("{name:>8}: n {} p50 {} p99 {} p99.9 {} max {} ns",
             h.len(), h.value_at_quantile(0.50), h.value_at_quantile(0.99),
             h.value_at_quantile(0.999), h.max());
}

/// The N largest samples, each with its sequence number, in fixed memory.
pub struct Worst<const N: usize> {
    v: [(u64, u64); N],   // (ns, seq)
    min_at: usize,        // where the smallest of the N is
}

impl<const N: usize> Worst<N> {
    pub fn new() -> Self { Worst { v: [(0, 0); N], min_at: 0 } }

    #[inline]
    pub fn offer(&mut self, ns: u64, seq: u64) {
        if ns <= self.v[self.min_at].0 { return; }     // nearly always: one compare
        self.v[self.min_at] = (ns, seq);               // replace the smallest kept
        self.min_at = (0..N).min_by_key(|&i| self.v[i].0).unwrap();  // N compares
    }

    /// After the run: the kept samples, slowest first.
    pub fn sorted(&self) -> Vec<(u64, u64)> {
        let mut v: Vec<_> = self.v.iter().copied().filter(|&(ns, _)| ns > 0).collect();
        v.sort_unstable_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        v
    }
}

impl<const N: usize> Default for Worst<N> {
    fn default() -> Self { Self::new() }
}

/// A stage's histogram and the sequence number of its slowest message, which
/// the histogram alone forgets.
pub struct Stage {
    pub h: Histogram<u64>,
    pub max: u64,
    pub max_seq: u64,
}

impl Stage {
    pub fn new() -> Self { Stage { h: new_hist(), max: 0, max_seq: 0 } }

    /// `saturating_record`, plus one compare to remember who set the max.
    #[inline]
    pub fn saturating_record(&mut self, ns: u64, seq: u64) {
        self.h.saturating_record(ns);
        if ns > self.max { self.max = ns; self.max_seq = seq; }
    }
}

impl Default for Stage {
    fn default() -> Self { Self::new() }
}

/// The strategy thread's histograms, made on its own core before the first message.
pub struct Hists {
    pub decode: Stage,            // t1 - t0, stages build only
    pub hop: Stage,               // t2 - t1, stages build only
    pub think: Stage,             // t3 - t2, stages build only
    pub total: Histogram<u64>,    // t3 - t0: the headline, every build
    pub worst: Worst<100>,        // the slowest totals, with their sequence numbers
    pub dropped: u64,             // orders refused by a full output ring: must stay 0
}

impl Hists {
    pub fn new() -> Self {
        Hists { decode: Stage::new(), hop: Stage::new(), think: Stage::new(),
                total: new_hist(), worst: Worst::new(), dropped: 0 }
    }
}

impl Default for Hists {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------- what crosses between threads

#[derive(Clone, Copy)]
#[repr(C)]
pub struct Stamped { pub t0: u64, pub t1: u64, pub u: Update }        // 48 bytes; Ring<Stamped, 4096> = 192 KB
#[derive(Clone, Copy)]
#[repr(C)]
pub struct StampedOrder { pub t3: u64, pub o: Order }                   // Ring<StampedOrder, 1024>

const _: () = assert!(size_of::<Stamped>() == 48);
const _: () = assert!(size_of::<StampedOrder>() == 32);

/// The networking chapter's `parse`, then the five fields the strategy uses,
/// copied into the data-structures chapter's 32-byte `Update`. Nothing allocated.
#[inline]
pub fn decode(buf: &[u8]) -> Option<Update> {
    let m = ch04_network::parse(buf)?;
    let side = match m.side { 0 => Side::Bid, 1 => Side::Ask, _ => return None };
    Some(Update { seq: m.seq.get(), price: m.price.get(), qty: m.qty.get(),
                  instrument: m.instrument.get(), side })
}

// ---------------------------------------------------------------- the replay file

/// One record: arrival time in ns since the file's first record, then 48 wire bytes.
pub const RECORD: usize = 56;
const PAGE: usize = 4096;

pub struct Record<'a> {
    pub t_arrival_ns: u64,
    pub msg: &'a [u8],     // the networking chapter's 48-byte message
}

enum Bytes { Map(memmap2::Mmap), Mem(Vec<u8>) }

/// The replay file, mapped, locked and touched before the run, so the feed
/// thread reads it with plain loads and no page fault lands inside a message.
pub struct Replay {
    bytes: Bytes,
    pub locked: bool,      // mlock succeeded
    pub pages: usize,      // pages touched once before the run
}

impl Replay {
    pub fn open(path: &Path) -> io::Result<Replay> {
        let file = std::fs::File::open(path)?;
        // SAFETY: the file is opened read-only and nothing in this process writes it;
        // a replay file changed by another process during a run is a broken run.
        let map = unsafe { memmap2::Mmap::map(&file)? };
        if !map.len().is_multiple_of(RECORD) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "not a whole number of 56-byte records"));
        }
        // keep the pages in RAM: a fault from disk is 100 µs or more
        // SAFETY: the range is exactly the mapping just made, alive as long as `map`.
        let locked = unsafe { libc::mlock(map.as_ptr().cast(), map.len()) } == 0;
        let mut r = Replay { bytes: Bytes::Map(map), locked, pages: 0 };
        r.pages = r.touch();
        Ok(r)
    }

    /// A replay held in memory, for tests: no file, no lock.
    pub fn from_bytes(v: Vec<u8>) -> Replay {
        assert!(v.len().is_multiple_of(RECORD), "not a whole number of 56-byte records");
        let mut r = Replay { bytes: Bytes::Mem(v), locked: false, pages: 0 };
        r.pages = r.touch();
        r
    }

    /// One load per page, so any page still missing faults now and not during the run.
    fn touch(&self) -> usize {
        let b = self.bytes();
        let mut sum = 0u64;
        for off in (0..b.len()).step_by(PAGE) {
            // SAFETY: off < b.len(); a volatile read so the load is not optimised away
            sum = sum.wrapping_add(unsafe { std::ptr::read_volatile(b.as_ptr().add(off)) } as u64);
        }
        std::hint::black_box(sum);
        b.len().div_ceil(PAGE)
    }

    pub fn bytes(&self) -> &[u8] {
        match &self.bytes { Bytes::Map(m) => m, Bytes::Mem(v) => v }
    }

    /// The number of records.
    pub fn len(&self) -> usize { self.bytes().len() / RECORD }
    pub fn is_empty(&self) -> bool { self.len() == 0 }

    /// Record k, counting from 0: it starts at byte 56 × k.
    pub fn record(&self, k: usize) -> Record<'_> {
        Self::split(&self.bytes()[k * RECORD..(k + 1) * RECORD])
    }

    #[inline]
    fn split(r: &[u8]) -> Record<'_> {
        Record { t_arrival_ns: u64::from_le_bytes(r[..8].try_into().unwrap()), msg: &r[8..] }
    }

    /// Every record, front to back: ordinary loads, no system call.
    pub fn records(&self) -> impl Iterator<Item = Record<'_>> {
        self.bytes().chunks_exact(RECORD).map(Self::split)
    }

    pub fn first_seq(&self) -> Option<u64> {
        self.records().next().and_then(|r| decode(r.msg)).map(|u| u.seq)
    }

    /// The mid of the first bid and the first ask in the file: where the book's window is centred.
    pub fn first_mid(&self) -> Option<Ticks> {
        let (mut bid, mut ask) = (None, None);
        for u in self.records().filter_map(|r| decode(r.msg)) {
            match u.side { Side::Bid => bid = bid.or(Some(u.price)), Side::Ask => ask = ask.or(Some(u.price)) }
            if let (Some(b), Some(a)) = (bid, ask) { return Some((b + a) / 2); }
        }
        None
    }
}

/// `mlockall(MCL_CURRENT | MCL_FUTURE)`: every page the process has or will
/// have stays in RAM and is filled in when mapped. Tried only as root or with
/// `ulimit -l unlimited`: under a finite limit the call itself succeeds, and
/// MCL_FUTURE then refuses the first mapping past the limit with EAGAIN, the
/// replay file's or a thread's stack. False when not tried or refused.
pub fn lock_memory() -> bool {
    let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: getrlimit writes only lim, a live local; geteuid and mlockall take no pointers.
    unsafe {
        // ponytail: root stands in for CAP_IPC_LOCK; a capability given by setcap, or a
        // finite limit large enough for the run, is not detected and runs unlocked
        let unlimited = libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut lim) == 0 && lim.rlim_cur == libc::RLIM_INFINITY;
        (unlimited || libc::geteuid() == 0) && libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) == 0
    }
}

/// The cores this process may run on: the ones its affinity mask allows.
pub fn available_cores() -> Vec<usize> {
    core_affinity::get_core_ids().unwrap_or_default().into_iter().map(|c| c.id).collect()
}

// ---------------------------------------------------------------- the strategy's three small types

/// One decision per batch: the update that closed it, the top after it, the quote wanted.
#[derive(Clone, Copy, Default)]
pub struct Decision {
    pub seq: u64,
    pub top: Top,
    pub want: [Option<(Ticks, Lots)>; 2],
}

/// The decisions file, held in memory until the run ends. Preallocated from
/// `--records`, the most batches a run can have, and touched once so that no
/// push faults a page in.
pub struct Log {
    pub v: Vec<Decision>,
    pub full: u64,             // pushes refused for want of room: must stay 0
}

impl Log {
    pub fn with_capacity(n: usize) -> Log {
        let mut v = Vec::with_capacity(n);
        v.resize(n, Decision::default());   // first touch now, not in the loop
        v.clear();                          // keeps the capacity
        Log { v, full: 0 }
    }

    #[inline]
    pub fn push(&mut self, seq: u64, top: Top, want: [Option<(Ticks, Lots)>; 2]) {
        if self.v.len() < self.v.capacity() {
            self.v.push(Decision { seq, top, want });   // within capacity: never reallocates
        } else {
            self.full += 1;
        }
    }

    /// One line per batch: `seq bid_px bid_qty ask_px ask_qty want_bid want_ask`, `-` for none.
    pub fn write_decisions(&self, mut w: impl Write) -> io::Result<()> {
        let lvl = |l: Option<(Ticks, Lots)>| l.map_or("- -".to_string(), |(p, q)| format!("{p} {q}"));
        let px = |l: Option<(Ticks, Lots)>| l.map_or("-".to_string(), |(p, _)| p.to_string());
        for d in &self.v {
            writeln!(w, "{} {} {} {} {}", d.seq, lvl(d.top.bid), lvl(d.top.ask), px(d.want[0]), px(d.want[1]))?;
        }
        Ok(())
    }

    /// One line per order the strategy sent: `seq side price qty`, where seq
    /// closed the batch that sent it. The quoter sends a side only when its
    /// wanted quote changed, so the orders are the changes between decisions.
    pub fn write_orders(&self, mut w: impl Write) -> io::Result<u64> {
        let (mut prev, mut n) = ([None, None], 0);
        for d in &self.v {
            for (i, side) in ["bid", "ask"].into_iter().enumerate() {
                if d.want[i] != prev[i] {
                    if let Some((p, q)) = d.want[i] { writeln!(w, "{} {side} {p} {q}", d.seq)?; n += 1; }
                }
            }
            prev = d.want;
        }
        Ok(n)
    }
}

/// What the main thread prepared for the strategy thread before spawning it.
pub struct Run {
    pub base: Ticks,             // ArrayBook's index 0: the window centred on the file's first mid
    pub quoter: Quoter,
    pub ns_per_tick: f64,        // the ticks-to-nanoseconds factor: 1 / calibrate()
    pub first_seq: u64,          // the file starts in order: nothing to join to
    pub warm_seq: u64,           // the first sequence number counted in the histograms
    pub log: Log,                // the decisions, one per batch
    pub done: Arc<AtomicBool>,   // the feed's flag, stored with Release after its last push
    pub core: Option<usize>,     // None: --strategy-core none, an unpinned thread
    pub stall_every: u64,        // --stall-every in ticks; 0: off
    pub stall_len: u64,          // each stall, 10 ms in ticks
    pub next_stall: u64,         // tick of the next stall
    pub backwards: Arc<AtomicU64>, // intervals that came out negative, shared with the gateway: must stay 0
}

impl Run {
    /// `later − earlier`, ticks to nanoseconds: one multiply, outside the timed
    /// region, after a compare that counts a backwards interval and gives it 0 ns.
    #[inline(always)]
    pub fn ns(&self, later: u64, earlier: u64) -> u64 {
        interval_ns(later, earlier, self.ns_per_tick, &self.backwards)
    }

    /// `--stall-every`: a 10 ms spin at the top of the loop, before the drain.
    /// One predicted branch when the flag is off.
    #[inline(always)]
    pub fn stall(&mut self) {
        if self.stall_every == 0 { return; }
        let t = now_unfenced();
        if t < self.next_stall { return; }
        while now_unfenced() < t + self.stall_len { spin_loop(); }   // the core is busy, not ours
        self.next_stall += self.stall_every;
    }
}

/// The most one drain hands the strategy: the input ring's capacity.
pub const BATCH: usize = 4096;

/// Each message's stamps from its pop until the batch's t3, because the
/// records must come after t3. Four arrays of 4,096 u64s, 128 KB.
pub struct Batch {
    pub t0: [u64; BATCH],
    pub t1: [u64; BATCH],
    pub t2: [u64; BATCH],
    pub seq: [u64; BATCH],
}

impl Batch {
    /// On the heap, zeroed, once, before the loop: 128 KB is too much for a stack frame.
    pub fn boxed() -> Box<Batch> {
        // SAFETY: all-zero bytes are a valid Batch, four arrays of u64.
        unsafe { Box::<Batch>::new_zeroed().assume_init() }
    }
}

/// What the strategy thread hands back through `join`.
pub struct Report {
    pub h: Hists,
    pub state: BookState,
    pub log: Log,
}

/// How the strategy makes its book, so that the book is a type parameter and
/// `--book` changes no line of the loop.
pub trait NewBook: Book + Send {
    fn new(base: Ticks) -> Self;
}

impl NewBook for ArrayBook {
    fn new(base: Ticks) -> Self { ArrayBook::new(base) }          // 65,536 levels a side
}

impl NewBook for TreeBook {
    fn new(_base: Ticks) -> Self { TreeBook::default() }          // nodes come and go with levels
}

impl NewBook for VecBook {
    fn new(_base: Ticks) -> Self { VecBook::with_capacity(LEVELS) } // preallocated: moves, never grows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worst_keeps_the_largest_with_their_seqs() {
        let mut w = Worst::<3>::new();
        for (ns, seq) in [(5, 1), (9, 2), (1, 3), (7, 4), (8, 5), (2, 6)] { w.offer(ns, seq); }
        assert_eq!(w.sorted(), vec![(9, 2), (8, 5), (7, 4)]);
    }

    #[test]
    fn stage_remembers_who_set_the_max() {
        let mut s = Stage::new();
        for (ns, seq) in [(10, 1), (300, 2), (20, 3)] { s.saturating_record(ns, seq); }
        assert_eq!((s.max, s.max_seq, s.h.len()), (300, 2, 3));
        s.saturating_record(5_000_000_000, 9);                 // above 1 s: counted at the top
        assert_eq!(s.h.len(), 4);
    }

    #[test]
    fn a_backwards_interval_reads_zero_and_is_counted() {
        let back = AtomicU64::new(0);
        assert_eq!(interval_ns(1_000, 400, 0.5, &back), 300);
        assert_eq!(interval_ns(400, 400, 0.5, &back), 0);
        assert_eq!(back.load(Relaxed), 0);
        assert_eq!(interval_ns(400, 1_000, 0.5, &back), 0);     // later below earlier: two clocks
        assert_eq!(back.load(Relaxed), 1);
        let mut s = Stage::new();
        s.saturating_record(interval_ns(400, 1_000, 0.5, &back), 7); // recorded as 0 ns, not as 1 s
        assert_eq!((s.h.len(), s.h.max(), back.load(Relaxed)), (1, 0, 2));
    }

    #[test]
    fn clock_is_monotonic_and_calibrates_near_the_kernels_rate() {
        let a = now();
        let b = now();
        assert!(b >= a);
        let tpn = calibrate();
        assert!(tpn > 0.5 && tpn < 10.0, "ticks per ns {tpn}");
    }

    #[test]
    fn decode_fills_the_32_byte_update() {
        let m = ch04_network::encode(77, 6_812_345, 512, 42, 1, 0);
        assert_eq!(decode(&m), Some(Update { seq: 77, price: 6_812_345, qty: 512, instrument: 42, side: Side::Ask }));
        let mut bad = m;
        bad[40] = 2;
        assert_eq!(decode(&bad), None);
    }

    #[test]
    fn orders_are_the_changes_between_decisions() {
        let mut log = Log::with_capacity(4);
        let t = Top { bid: Some((100, 1)), ask: Some((101, 1)) };
        log.push(1, t, [Some((98, 5)), Some((103, 5))]);
        log.push(2, t, [Some((98, 5)), Some((103, 5))]);       // nothing changed: no order
        log.push(3, t, [Some((99, 5)), Some((103, 5))]);       // the bid moved
        let mut out = Vec::new();
        assert_eq!(log.write_orders(&mut out).unwrap(), 3);
        assert_eq!(String::from_utf8(out).unwrap(), "1 bid 98 5\n1 ask 103 5\n3 bid 99 5\n");
        log.push(4, t, [None, None]);
        log.push(5, t, [None, None]);                          // past capacity: refused, counted
        assert_eq!((log.v.len(), log.full), (4, 1));
    }
}

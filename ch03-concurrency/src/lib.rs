//! Chapter 3, Sharing between threads: the chapter's code, and the clock,
//! histograms, pinning and topology that its four labs share.
//!
//! One module per mechanism, in the chapter's order:
//!
//! - [`atomics`]: section 3.3, the data race Rust refuses and the order-id counter
//! - [`litmus`]: sections 3.4 and 3.5 and Lab 3.1, message passing and store buffering
//! - [`quote`]: section 3.5, `QuoteCell`, the quote published properly
//! - [`futex`]: section 3.6, the standard mutex's `lock` and `unlock`, simplified
//! - [`freelist`]: section 3.7, `FreeList` and its tagged `pop`
//! - [`seqlock`]: section 3.8, `SeqLock`
//! - [`spsc`]: section 3.8 and Lab 3.4, `Ring<N>`, the 16-slot ring proved with loom
//! - [`OwnCounter`]: section 3.9, a counter with one writer
//! - [`sync`]: the atomic and cell types, from loom under `cfg(loom)` and from std otherwise
//! - [`topo`]: pinning, and what two cores share (siblings, one L3, two L3s, two sockets)

use hdrhistogram::Histogram;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed}};
use std::time::{Duration, Instant};

pub mod atomics;
pub mod freelist;
#[cfg(target_os = "linux")]
pub mod futex;
pub mod litmus;
pub mod quote;
pub mod seqlock;
pub mod spsc;
pub mod sync;
pub mod topo;

// ---------------------------------------------------------------- the clock

/// The TSC, read once every earlier instruction has finished.
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub fn now() -> u64 {
    use core::arch::x86_64::{_mm_lfence, _rdtsc};
    // SAFETY: lfence and rdtsc exist on every x86_64 part and touch no memory.
    unsafe {
        _mm_lfence(); // wait for earlier instructions: do not read early
        _rdtsc()
    }
}

/// The virtual counter, read once earlier instructions have finished; Lab 3.1
/// asks for a run on AArch64 as well, so the crate builds there too.
#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub fn now() -> u64 {
    let t: u64;
    // SAFETY: isb and a read of cntvct_el0, which Linux lets user space read; no memory is touched.
    unsafe { core::arch::asm!("isb", "mrs {t}, cntvct_el0", t = out(reg) t, options(nomem, nostack, preserves_flags)) };
    t
}

/// Anywhere else: nanoseconds from the first call, through the OS clock.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
#[inline(always)]
pub fn now() -> u64 {
    static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Clock ticks per nanosecond, measured once at startup against CLOCK_MONOTONIC.
pub fn calibrate() -> f64 {
    let (i0, t0) = (Instant::now(), now());
    while i0.elapsed() < Duration::from_millis(200) {} // spin for 200 ms
    let (i1, t1) = (Instant::now(), now());
    (t1 - t0) as f64 / (i1 - i0).as_nanos() as f64
}

/// Ticks to nanoseconds.
#[inline(always)]
pub fn ns(ticks: u64, ticks_per_ns: f64) -> u64 {
    (ticks as f64 / ticks_per_ns) as u64
}

/// The p50 of back-to-back `now()` pairs, in ns: the clock's own share of every
/// interval it closes. Printed for reference; no lab here subtracts it.
pub fn clock_pair_p50_ns(pairs: usize, ticks_per_ns: f64) -> u64 {
    let mut h = new_hist();
    for _ in 0..pairs {
        let a = now();
        let b = now();
        h.saturating_record(ns(b - a, ticks_per_ns).max(1));
    }
    h.value_at_quantile(0.50)
}

// ---------------------------------------------------------------- histograms

/// Before the run: 1 ns to 1 s at 3 significant figures, every byte it will use.
pub fn new_hist() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).expect("1 ns..=1 s at 3 figures is a valid histogram")
}

/// count, p50, p99, p99.9 and max, in ns; never a mean.
pub fn report(label: &str, h: &Histogram<u64>) {
    println!("  {label}: n {} p50 {} p99 {} p99.9 {} max {} ns",
             h.len(), h.value_at_quantile(0.50), h.value_at_quantile(0.99),
             h.value_at_quantile(0.999), h.max());
}

/// The same for a histogram of batch times, each divided by the batch size:
/// ns per operation, batch by batch, with one decimal.
pub fn report_per(label: &str, h: &Histogram<u64>, per: u64) {
    let q = |x: u64| x as f64 / per as f64;
    println!("  {label}: n {} p50 {:.1} p99 {:.1} p99.9 {:.1} max {:.1} ns",
             h.len(), q(h.value_at_quantile(0.50)), q(h.value_at_quantile(0.99)),
             q(h.value_at_quantile(0.999)), q(h.max()));
}

// ---------------------------------------------------------------- what the labs share

/// One value on 128 bytes of its own: two lines, because x86's adjacent-line
/// prefetcher pulls lines in pairs (section 3.9, `CachePadded`).
#[repr(align(128))]
#[derive(Default, Debug)]
pub struct Padded<T>(pub T);

impl<T> Deref for Padded<T> {
    type Target = T;
    #[inline(always)]
    fn deref(&self) -> &T { &self.0 }
}

/// A barrier that spins. `std::sync::Barrier` parks its threads, the two stores
/// then land microseconds apart, and store buffering's window is never hit.
pub struct SpinBarrier {
    arrived: Padded<AtomicU64>,
    parties: u64,
}

impl SpinBarrier {
    pub fn new(parties: u64) -> Self { SpinBarrier { arrived: Padded(AtomicU64::new(0)), parties } }

    /// `gen` is the caller's own count of barriers passed, starting at 0.
    #[inline(always)]
    pub fn wait(&self, gen: &mut u64) {
        *gen += 1;
        self.arrived.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        // no spin_loop here: pause would add ~140 cycles of slack between the two threads
        while self.arrived.load(Acquire) < *gen * self.parties {}
    }
}

/// The chapter's prediction, printed before any number is measured.
pub fn predict(lines: &[&str]) {
    println!("Predict first. The chapter's prediction:");
    for l in lines {
        println!("  {l}");
    }
    println!("Write your own down before reading the numbers below.\n");
}

/// A number from the command line; `_` separators allowed, as in 100_000_000.
pub fn parse_num(s: &str, flag: &str) -> Result<u64, String> {
    s.replace('_', "").parse().map_err(|_| format!("{flag}: not a number: {s}"))
}

/// `--cores 2,3`: exactly `n` comma-separated core numbers.
pub fn parse_cores(s: &str, n: usize, flag: &str) -> Result<Vec<usize>, String> {
    let v: Vec<usize> = s.split(',').map(|c| c.trim().parse::<usize>())
        .collect::<Result<_, _>>()
        .map_err(|_| format!("{flag}: core numbers separated by commas, such as 2,3: {s}"))?;
    if v.len() != n {
        return Err(format!("{flag}: wants {n} cores, got {}: {s}", v.len()));
    }
    Ok(v)
}

/// Thousands separators, for counts a reader compares by eye.
pub fn commas(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------- 3.9: a counter with one writer

#[repr(align(128))]
pub struct OwnCounter(AtomicU64);

impl OwnCounter {
    // called only by the owning thread: a load and a store, no lock prefix
    pub fn bump(&self) { self.0.store(self.0.load(Relaxed) + 1, Relaxed); }
    // called by anyone
    pub fn get(&self) -> u64 { self.0.load(Relaxed) }
}

impl OwnCounter {
    pub fn new() -> Self { OwnCounter(AtomicU64::new(0)) }
}

impl Default for OwnCounter {
    fn default() -> Self { Self::new() }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn padded_values_sit_on_lines_of_their_own() {
        assert_eq!(std::mem::align_of::<Padded<AtomicU64>>(), 128);
        assert_eq!(std::mem::size_of::<OwnCounter>(), 128);
        let two = [Padded(AtomicU64::new(0)), Padded(AtomicU64::new(0))];
        let (a, b) = (&two[0] as *const _ as usize, &two[1] as *const _ as usize);
        assert_eq!(b - a, 128);
    }

    #[test]
    fn own_counters_summed_by_the_reader() {
        let counters: Vec<OwnCounter> = (0..4).map(|_| OwnCounter::new()).collect();
        std::thread::scope(|s| {
            for c in &counters {
                s.spawn(move || for _ in 0..100_000 { c.bump() });
            }
        });
        assert_eq!(counters.iter().map(OwnCounter::get).sum::<u64>(), 400_000);
    }

    #[test]
    fn spin_barrier_lines_two_threads_up() {
        let bar = SpinBarrier::new(2);
        let hits = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..2 {
                s.spawn(|| {
                    let mut gen = 0;
                    for round in 1..=1000u64 {
                        hits.fetch_add(1, Relaxed);
                        bar.wait(&mut gen);
                        assert!(hits.load(Relaxed) >= 2 * round); // both arrived before either left
                        bar.wait(&mut gen);
                    }
                });
            }
        });
    }

    #[test]
    fn command_line_helpers() {
        assert_eq!(parse_num("100_000_000", "--iters"), Ok(100_000_000));
        assert!(parse_num("ten", "--iters").is_err());
        assert_eq!(parse_cores("2,3", 2, "--cores"), Ok(vec![2, 3]));
        assert!(parse_cores("2", 2, "--cores").is_err());
        assert_eq!(commas(40_000_000), "40,000,000");
        assert_eq!(commas(999), "999");
    }
}

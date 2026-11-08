//! The labs' clock and their reports: rdtsc behind a fence, calibrated once
//! against CLOCK_MONOTONIC_RAW, and an hdrhistogram per run printed as count,
//! p50, p99, p99.9 and max, never a mean.

use core::arch::x86_64::{_mm_lfence, _rdtsc};

use hdrhistogram::Histogram;

/// The TSC, read once every earlier instruction has finished.
#[inline(always)]
pub fn now() -> u64 {
    // SAFETY: lfence and rdtsc exist on every x86_64 part and touch no memory.
    unsafe {
        _mm_lfence(); // wait for earlier instructions: do not read early
        _rdtsc()
    }
}

/// TSC ticks per nanosecond, measured once at startup against
/// CLOCK_MONOTONIC_RAW, the kernel clock the time daemon never slews.
pub fn calibrate() -> f64 {
    let raw = || { // CLOCK_MONOTONIC_RAW, in ns
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime only writes ts, a live local.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    };
    let (n0, t0) = (raw(), now());
    while raw() - n0 < 200_000_000 {} // spin for 200 ms
    let (n1, t1) = (raw(), now());
    (t1 - t0) as f64 / (n1 - n0) as f64
}

/// The p50 of back-to-back `now()` pairs, in ticks: the clock's own share of
/// every interval it closes.
pub fn clock_pair_p50(pairs: usize) -> u64 {
    let mut h = new_hist();
    for _ in 0..pairs {
        let a = now();
        let b = now();
        h.saturating_record((b - a).max(1));
    }
    h.value_at_quantile(0.50)
}

/// Before the run: every byte the histogram will ever use.
pub fn new_hist() -> Histogram<u64> {
    // 1 ns to 1 s at 3 significant figures; recording never allocates
    Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).expect("valid bounds")
}

/// Ticks to whole nanoseconds, at least 1 so the histogram keeps the sample.
#[inline(always)]
pub fn ns(ticks: u64, ticks_per_ns: f64) -> u64 {
    ((ticks as f64 / ticks_per_ns) as u64).max(1)
}

/// A nanosecond count the way the chapter writes it: ns up to 10 µs, then µs
/// up to 1 ms, then ms.
pub fn human(ns: u64) -> String {
    if ns < 10_000 { format!("{ns} ns") }
    else if ns < 1_000_000 { format!("{:.1} µs", ns as f64 / 1e3) }
    else { format!("{:.2} ms", ns as f64 / 1e6) }
}

/// One line: count, p50, p99, p99.9, max.
pub fn report(name: &str, h: &Histogram<u64>) {
    println!("{name}: n {}  p50 {}  p99 {}  p99.9 {}  max {}",
             h.len(), human(h.value_at_quantile(0.50)), human(h.value_at_quantile(0.99)),
             human(h.value_at_quantile(0.999)), human(h.max()));
}

/// The prediction the chapter asks the reader to write down before measuring.
pub fn predict(lines: &[&str]) {
    println!("Predict first (the chapter's prediction):");
    for l in lines { println!("  {l}"); }
}

/// A number with thousands separators, as the chapter prints its counts.
pub fn commas(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}

/// A count flag's value: digits, with `_` or `,` allowed as separators.
pub fn count(s: &str) -> Option<u64> {
    s.replace(['_', ','], "").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_runs_forward_and_calibrates_near_the_tsc_rate() {
        let a = now();
        let b = now();
        assert!(b >= a);
        let tpn = calibrate();
        assert!(tpn > 0.5 && tpn < 10.0, "{tpn} ticks/ns");
    }

    #[test]
    fn formatting() {
        assert_eq!(commas(1_873_465_902), "1,873,465,902");
        assert_eq!(commas(999), "999");
        assert_eq!(human(150), "150 ns");
        assert_eq!(human(18_234), "18.2 µs");
        assert_eq!(human(40_012_000), "40.01 ms");
        assert_eq!(count("1_000,000"), Some(1_000_000));
        assert_eq!(count("x"), None);
    }
}

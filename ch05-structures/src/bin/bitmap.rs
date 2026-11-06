//! bitmap: exercise 5, the stretch. The next set bit above index i in one
//! 64-bit bitmap, by masking off bits 0 to i and calling `trailing_zeros`,
//! against a linear scan over `[bool; 64]`, with the distance to the next set
//! bit fixed at each of 1 to 63, then random. Both are `#[inline(never)]`
//! functions of the library, so a call's cost is in both and `cargo asm` can
//! find them.

use ch05_structures::lab::{self, count, fmt_count, fmt_ns, new_hist, now, summarize, Clock, Rng, Summary};
use ch05_structures::{bitscan_above, flagscan_above};
use hdrhistogram::Histogram;
use std::hint::black_box;

const HELP: &str = "\
bitmap: exercise 5, trailing_zeros against a scan over [bool; 64]

usage: bitmap [both|bitscan|scan] [all|fixed|random] [--calls N] [--core N|none] [--seed S]

  both|bitscan|scan  which search (default both)
  all|fixed|random   the distance to the next set bit: each of 1 to 63 held fixed,
                     random from 1 to 63, or both (default all)
  --calls N      calls per distance, and for the random run (default 1,000,000)
  --core N       pin to core N (default 3); `none` for unpinned
  --seed S       the random distances' seed (default 7)

Confirm the instructions (cargo install cargo-show-asm):
  cargo asm -p ch05-structures --lib bitscan_above      a tzcnt (or rep bsf), no loop
  cargo asm -p ch05-structures --lib flagscan_above     a cmp and a jne per flag
  RUSTFLAGS=\"-C target-cpu=native\" for tzcnt rather than bsf.
and the branches (needs perf):
  perf stat -e branches,branch-misses -- taskset -c 3 ./bitmap scan random
  perf stat -e branches,branch-misses -- taskset -c 3 ./bitmap bitscan random";

/// Calls a clock sample covers.
const BLOCK: u64 = 1_000;

/// One search per call, the bitmaps and flags for each distance in L1, the
/// distance for call k read from `dist`. Returns ns per block of calls.
fn time(calls: u64, dist: &[u8; 4096], clock: &Clock, mut f: impl FnMut(usize) -> usize) -> Histogram<u64> {
    let mut hist = new_hist();
    let mut sum = 0usize;
    for b in 0..calls / BLOCK {
        let t0 = now();
        for k in 0..BLOCK {
            let d = dist[((b * BLOCK + k) & 4095) as usize] as usize;
            sum += f(d);
        }
        let t1 = now();
        hist.saturating_record(clock.ns(t1 - t0));
    }
    black_box(sum);
    hist
}

fn main() {
    let (mut which, mut mode, mut calls, mut core, mut seed) = (String::from("both"), String::from("all"), 1_000_000u64, Some(3), 7u64);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "both" | "bitscan" | "scan" => which = a,
            "all" | "fixed" | "random" => mode = a,
            "--calls" => calls = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--calls: a count", HELP)),
            "--core" => core = lab::core(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--core: a core or none", HELP)),
            "--seed" => seed = lab::value(&mut args, &a, HELP).parse().unwrap_or_else(|_| lab::usage("--seed: an integer", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }
    if calls < BLOCK { lab::usage("--calls must be at least 1,000", HELP); }

    lab::predict(&[
        "the bit-scan costs about 1 ns at any distance. The scan costs about 1 ns per 8 bytes plus a",
        "mispredict of 15-20 cycles at the exit, so at a fixed distance, where the predictor learns",
        "the exit, the two cross near 8 buckets; at random distances the scan pays 4-13 ns and never",
        "catches up. perf stat: the scan misses about once a call at random distances, the bit-scan never.",
    ]);

    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("{}", lab::pin_or_warn("bitmap", core));
    println!("{} calls a run, timed in blocks of {BLOCK}; per call = block / {BLOCK}, a call's own cost included\n", fmt_count(calls));

    // For distance d: bit 0 (the index i) and bit d set; flags the same.
    let words: [u64; 64] = std::array::from_fn(|d| 1 | (1u64 << d));
    let flags: [[bool; 64]; 64] = std::array::from_fn(|d| std::array::from_fn(|j| j == 0 || j == d));
    let (words, flags) = (black_box(words), black_box(flags));
    let bs = |d: usize| bitscan_above(words[d], black_box(0)) as usize;
    let sc = |d: usize| flagscan_above(&flags[d], black_box(0));
    debug_assert!((1..64).all(|d| bs(d) == d && sc(d) == d));
    let (do_bs, do_sc) = (which != "scan", which != "bitscan");
    let s = |h: &Histogram<u64>| summarize(h, BLOCK as f64, clock.pair_ns());

    if mode != "random" {
        println!("fixed distance: ns a call, p50 / p99 / max");
        println!("  {:>3}  {:>24}  {:>24}", "d", if do_bs { "bitscan" } else { "" }, if do_sc { "scan [bool; 64]" } else { "" });
        let mut cross = None;
        for d in 1..64usize {
            let dist = [d as u8; 4096];
            let b = do_bs.then(|| s(&time(calls, &dist, &clock, bs)));
            let c = do_sc.then(|| s(&time(calls, &dist, &clock, sc)));
            let cell = |x: Option<Summary>| x.map_or(String::new(), |x| format!("{} / {} / {}", fmt_ns(x.p50), fmt_ns(x.p99), fmt_ns(x.max)));
            println!("  {d:>3}  {:>24}  {:>24}", cell(b), cell(c));
            if let (Some(b), Some(c), None) = (b, c, cross) { if c.p50 > b.p50 { cross = Some(d); } }
        }
        if do_bs && do_sc {
            match cross {
                Some(d) => println!("  the scan's p50 first exceeds the bit-scan's at d = {d} (the chapter: near 8)\n"),
                None => println!("  the scan never exceeded the bit-scan at a fixed distance\n"),
            }
        }
    }

    if mode != "fixed" {
        let mut rng = Rng(seed);
        let dist: [u8; 4096] = std::array::from_fn(|_| 1 + rng.below(63) as u8);
        let mean = dist.iter().map(|&d| d as f64).sum::<f64>() / 4096.0;
        println!("random distance, 1 to 63 (mean {mean:.1}), a new one each call:");
        if do_bs { println!("  bitscan  {}", s(&time(calls, &dist, &clock, bs)).line()); }
        if do_sc { println!("  scan     {}", s(&time(calls, &dist, &clock, sc)).line()); }
    }
}

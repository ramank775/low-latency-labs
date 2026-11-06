//! lookup: exercise 3. Ten million lookups of instrument id to `Position` over
//! random ids: `HashMap<u32, Position>` with the default hasher (SipHash-1-3),
//! then with the hand-written FxHash, then `Positions`, a `Vec<Position>`
//! indexed by id. Each lookup's id is computed from the previous result, so
//! the core cannot overlap them, and each result goes through `black_box`.
//! `none` runs the same loop with no lookup in it: the loop's own cost, which
//! the other three are reported net of.

use ch05_structures::lab::{self, count, fmt_count, fmt_ns, new_hist, now, summarize, Clock, Rng, Summary};
use ch05_structures::{FxBuild, Position, Positions};
use hdrhistogram::Histogram;
use std::collections::HashMap;
use std::hint::black_box;

const HELP: &str = "\
lookup: exercise 3, HashMap (SipHash), HashMap (FxHash) and Vec<Position> by id

usage: lookup [all|map|fx|array|none] [LOOKUPS] [--ids K] [--core N|none] [--seed S]

  map            HashMap<u32, Position>, the default hasher
  fx             HashMap<u32, Position> with the hand-written FxHash
  array          Positions: Vec<Position> indexed by id
  none           the loop alone: the id arithmetic and black_box, no lookup
  all            none, map, fx and array at 64 ids, then again at 100,000 (default)
  LOOKUPS        lookups a run (default 10,000,000)
  --ids K        random ids below K (default 64; all runs both 64 and 100,000)
  --core N       pin to core N (default 3); `none` for unpinned
  --seed S       the ids' seed (default 7)

The chapter runs each under perf (needs perf, and root or
kernel.perf_event_paranoid <= 2 for a process of your own):
  perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./lookup map 10000000
  perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./lookup fx 10000000
  perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./lookup array 10000000
and divides each count by 10,000,000; subtract `./lookup none 10000000`.
cargo asm --lib qty_of shows the array's whole lookup.";

/// Lookups a clock sample covers: two rdtsc reads per thousand lookups.
const BLOCK: u64 = 1_000;

/// The dependent chain: the next id is a function of the last result, so
/// each lookup waits for the one before, as a real strategy's do not overlap.
#[inline(always)]
fn chain(lookups: u64, ids: u64, seed: u64, clock: &Clock, mut look: impl FnMut(u32) -> i64) -> (Histogram<u64>, u64, i64) {
    let mut hist = new_hist();
    let (mut state, mut q, mut sum) = (seed | 1, 0i64, 0i64);
    let start = now();
    for _ in 0..lookups / BLOCK {
        let t0 = now();
        for _ in 0..BLOCK {
            state = state.wrapping_mul(0x5851_f42d_4c95_7f2d).wrapping_add(q as u64 | 1); // waits for q
            let id = (((state >> 32) * ids) >> 32) as u32;                                 // below ids, no division
            q = black_box(look(id));
            sum = sum.wrapping_add(q);
        }
        let t1 = now();
        hist.saturating_record(clock.ns(t1 - t0));
    }
    (hist, clock.ns(now() - start), sum)
}

fn main() {
    let (mut which, mut lookups, mut ids, mut core, mut seed) = (String::from("all"), 10_000_000u64, None, Some(3), 7u64);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "all" | "map" | "fx" | "array" | "none" => which = a,
            "--ids" => ids = Some(count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--ids: a count", HELP))),
            "--core" => core = lab::core(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--core: a core or none", HELP)),
            "--seed" => seed = lab::value(&mut args, &a, HELP).parse().unwrap_or_else(|_| lab::usage("--seed: an integer", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            n if count(n).is_some() => lookups = count(n).unwrap_or_default(),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }
    if lookups < BLOCK { lab::usage("LOOKUPS must be at least 1,000", HELP); }
    if ids.is_some_and(|k| k == 0 || k > u32::MAX as u64) { lab::usage("--ids: between 1 and 2^32", HELP); }

    lab::predict(&[
        "at 64 ids: map 20-50 ns, fx 5-15 ns, array 1 ns a lookup; 200-500 ms, 50-150 ms and about",
        "10 ms a run of 10 million. Misses near zero for all three: 2 KB of positions and 5,264",
        "bytes of table sit in L1, so the difference is in instructions.",
        "at 100,000 ids the table sits in L3: map 50-100 ns against the array's 10-40, two lines to one.",
    ]);

    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("{}", lab::pin_or_warn("lookup", core));
    println!("{} lookups a run, dependent, timed in blocks of {BLOCK}; per lookup = block / {BLOCK}\n", fmt_count(lookups));

    let sizes: Vec<u64> = match (ids, which.as_str()) {
        (Some(k), _) => vec![k],
        (None, "all") => vec![64, 100_000],
        (None, _) => vec![64],
    };
    let modes: Vec<&str> = if which == "all" { vec!["none", "map", "fx", "array"] } else { vec![which.as_str()] };

    for &k in &sizes {
        // Every structure holds the same positions, qty a random 24-bit number per id.
        let mut rng = Rng(seed ^ 0xabcd);
        let qty: Vec<i64> = (0..k).map(|_| (rng.next_u64() >> 40) as i64).collect();
        let pos = |id: u32| { let mut p = Position::default(); p.instrument = id; p.qty = qty[id as usize]; p };
        println!("ids below {} ({} positions: {} KB as an array)", fmt_count(k), fmt_count(k), k * 32 / 1024);
        let mut baseline: Option<Summary> = None;
        for &m in &modes {
            let (hist, total_ns, sum) = match m {
                "map" => {
                    let mut map: HashMap<u32, Position> = HashMap::with_capacity(k as usize);
                    for id in 0..k as u32 { map.insert(id, pos(id)); }
                    chain(lookups, k, seed, &clock, |id| map.get(&id).map_or(0, |p| p.qty))
                }
                "fx" => {
                    let mut map: HashMap<u32, Position, FxBuild> = HashMap::with_capacity_and_hasher(k as usize, FxBuild::default());
                    for id in 0..k as u32 { map.insert(id, pos(id)); }
                    chain(lookups, k, seed, &clock, |id| map.get(&id).map_or(0, |p| p.qty))
                }
                "array" => {
                    let mut p = Positions::new(k as usize);
                    for id in 0..k as u32 { p.get_mut(id).qty = qty[id as usize]; }
                    chain(lookups, k, seed, &clock, |id| p.qty(id))
                }
                _ => chain(lookups, k, seed, &clock, |id| id as i64),
            };
            let s = summarize(&hist, BLOCK as f64, clock.pair_ns());
            let net = baseline.map(|b| format!(", net of the loop {} ns", fmt_ns((s.p50 - b.p50).max(0.0)))).unwrap_or_default();
            println!("  {m:<6} {:>8.1} ms in all; per lookup {}{net}   [checksum {}]", total_ns as f64 / 1e6, s.line(), sum);
            if m == "none" { baseline = Some(s); }
        }
        println!();
    }
}

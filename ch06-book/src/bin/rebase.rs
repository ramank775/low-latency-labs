//! rebase: exercise 4, the stretch. A price leaving the dense array's window
//! without a full rebuild: the window made a ring (`RingBook`), so a re-base
//! is a change of origin plus clearing the slots that change ends, measured
//! against the copy re-base (`ArrayBook::rebase_copy`, a `memmove` of up to
//! 512 KB a side) and against a rebuild from the levels of the reference book
//! (`ArrayBook::rebuild`), for a step of a few levels and a jump of the whole
//! window. Then a stream whose price walks out of the window: the plain array
//! goes wrong, the ring and the copy re-base keep it right.

use ch05_structures::lab::{self, count, fmt_count, fmt_ns, new_hist, now, summarize, Clock};
use ch06_book::{gen, ArrayBook, Book, RingBook, Side, Ticks, TreeBook, Update, LEVELS};
use hdrhistogram::Histogram;

const HELP: &str = "\
rebase: exercise 4 (stretch), the window as a ring against the copy re-base and a rebuild

usage: rebase [--records N] [--seed S] [--step K] [--reps R] [--drift-records N]
              [--margin M] [--core N|none]

  --records N    updates that build the book before the re-bases (default 200,000)
  --seed S       the generator's seed (default 7)
  --step K       the small re-base, in levels (default 4)
  --reps R       re-bases timed for each size and method, alternately up and down
                 (default 1,000)
  --drift-records N  the walking stream of part 2 (default 1,000,000, a fifth of
                 them moving the best price, every move upward)
  --margin M     part 2 re-centres the window on the mid when a best price comes
                 within M levels of an edge (default 16,384, a quarter window)
  --core N       pin to core N (default 3); `none` for unpinned";

/// Levels the reference holds inside [base, base + LEVELS) that `qty` does not hold.
fn holes(levels: &[Update], base: Ticks, qty: impl Fn(Side, Ticks) -> Option<i64>) -> usize {
    levels.iter().filter(|l| l.price >= base && l.price < base + LEVELS as Ticks && qty(l.side, l.price) != Some(l.qty)).count()
}

fn shifted(levels: &[Update], by: Ticks) -> Vec<Update> {
    levels.iter().map(|l| Update { price: l.price + by, ..*l }).collect()
}

fn main() {
    let (mut records, mut seed, mut step, mut reps, mut drift, mut margin, mut core) =
        (200_000u64, 7u64, 4i64, 1_000u64, 1_000_000u64, (LEVELS / 4) as i64, Some(3));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let n = |args: &mut std::iter::Skip<std::env::Args>| count(&lab::value(args, &a, HELP)).unwrap_or_else(|| lab::usage(&format!("{a}: a count"), HELP));
        match a.as_str() {
            "--records" => records = n(&mut args),
            "--seed" => seed = n(&mut args),
            "--step" => step = n(&mut args).clamp(1, LEVELS as u64 - 1) as i64,
            "--reps" => reps = n(&mut args).max(2),
            "--drift-records" => drift = n(&mut args),
            "--margin" => margin = n(&mut args).clamp(1, LEVELS as u64 / 2 - 1) as i64,
            "--core" => core = lab::core(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--core: a core or none", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }

    lab::predict(&[
        "the copy re-base is a 1 MB memmove, 50-150 µs (50-150 budgets) whatever the step; the ring's is",
        "a new origin and the cleared slots, near zero for a step of a few levels; a rebuild clears the",
        "whole window and applies every level, and in production waits 70-350 ms for the snapshot. Write",
        "down which of the three wins for a step and which for a jump of the whole window.",
    ]);
    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("{}", lab::pin_or_warn("rebase", core));

    // ---------------------------------------------------------------- 1. three ways to move the window
    let (updates, _) = gen::generate(&gen::Params { records, seed, ..Default::default() });
    let base0 = gen::first_mid(&updates).unwrap_or(updates[0].price) - (LEVELS / 2) as Ticks;
    let mut reference = TreeBook::default();
    let (mut copy, mut ring, mut rebuilt) = (ArrayBook::new(base0), RingBook::new(base0), ArrayBook::new(base0));
    for u in &updates { reference.apply(u); copy.apply(u); ring.apply(u); rebuilt.apply(u); }
    let levels = reference.snapshot();
    let (bids, asks) = reference.depth();
    println!("the book: {} updates, {bids} bid and {asks} ask levels, window {base0}..{}\n", fmt_count(records), base0 + LEVELS as Ticks);

    println!("1. one re-base, timed, {} times each way alternately; µs:", fmt_count(reps));
    for (label, d) in [(format!("a step of {step} levels"), step), ("a jump of the whole window".to_string(), LEVELS as Ticks)] {
        let mut h: [Histogram<u64>; 3] = [new_hist(), new_hist(), new_hist()];
        let up = shifted(&levels, d);                              // for the jump: the market moved with the window
        for r in 0..reps {
            let dir = if r % 2 == 0 { d } else { -d };
            let t0 = now(); copy.rebase_copy(copy.base() + dir); let t1 = now();
            ring.rebase(ring.base() + dir); let t2 = now();
            let lv = if r % 2 == 0 && d >= LEVELS as Ticks { &up } else { &levels };
            let t3 = now(); rebuilt.rebuild(rebuilt.base() + dir, lv); let t4 = now();
            for (hh, ticks) in h.iter_mut().zip([t1 - t0, t2 - t1, t4 - t3]) { hh.saturating_record(clock.ns(ticks)); }
        }
        println!("  {label}:");
        for (name, hh) in ["copy (memmove)", "ring", "rebuild"].iter().zip(&h) {
            let s = summarize(hh, 1e3, clock.pair_ns());
            println!("    {name:<15} p50 {:>8} p99 {:>8} max {:>8} µs  (n {})", fmt_ns(s.p50), fmt_ns(s.p99), fmt_ns(s.max), fmt_count(s.n));
        }
        // what one re-base up leaves, against the venue's book in the new window
        let (mut c, mut g) = (ArrayBook::new(base0), RingBook::new(base0));
        for u in &updates { c.apply(u); g.apply(u); }
        let nb = base0 + d;
        c.rebase_copy(nb);
        g.rebase(nb);
        let venue = if d >= LEVELS as Ticks { &up } else { &levels };
        let mut r = ArrayBook::new(base0);
        r.rebuild(nb, venue);
        println!("    levels of the venue's book inside the new window that each lacks: copy {}, ring {}, rebuild {}{}",
                 holes(venue, nb, |s, p| c.qty_at(s, p)), holes(venue, nb, |s, p| g.qty_at(s, p)), holes(venue, nb, |s, p| r.qty_at(s, p)),
                 if d >= LEVELS as Ticks { " (the price jumped a whole window: only a snapshot knows the new levels)" } else { "" });
    }

    // ---------------------------------------------------------------- 2. a price that walks out of the window
    let (walk, _) = gen::generate(&gen::Params { records: drift, seed, moves: 0.2, trend: true, ..Default::default() });
    let wbase = gen::first_mid(&walk).unwrap_or(walk[0].price) - (LEVELS / 2) as Ticks;
    let (mut venue, mut plain, mut copy, mut ring) = (TreeBook::default(), ArrayBook::new(wbase), ArrayBook::new(wbase), RingBook::new(wbase));
    let (mut hc, mut hr) = (new_hist(), new_hist());
    let mut wrong = [0u64; 3];
    let near = |base: Ticks, b: &dyn Book| {
        let edge = |p: Ticks| p < base + margin as Ticks || p >= base + (LEVELS as i64 - margin) as Ticks;
        b.best_bid().is_some_and(|l| edge(l.0)) || b.best_ask().is_some_and(|l| edge(l.0))
    };
    let mid = |b: &dyn Book| match (b.best_bid(), b.best_ask()) { (Some(x), Some(y)) => Some((x.0 + y.0) / 2), _ => None };
    for u in &walk {
        venue.apply(u);
        plain.apply(u);
        copy.apply(u);
        ring.apply(u);
        if near(copy.base(), &copy) {                              // the handler's check: the best nears the edge
            if let Some(m) = mid(&copy) { let t0 = now(); copy.rebase_copy(m - (LEVELS / 2) as Ticks); hc.saturating_record(clock.ns(now() - t0)); }
        }
        if near(ring.base(), &ring) {
            if let Some(m) = mid(&ring) { let t0 = now(); ring.rebase(m - (LEVELS / 2) as Ticks); hr.saturating_record(clock.ns(now() - t0)); }
        }
        let v = venue.top();
        for (w, top) in wrong.iter_mut().zip([plain.top(), copy.top(), ring.top()]) { *w += (top != v) as u64; }
    }
    let (first, last) = (wbase + (LEVELS / 2) as Ticks, venue.best_bid().map_or(0, |b| b.0));
    println!("\n2. A price that walks out: {} updates, the best bid from {first} to {last}, {} ticks, \
              against a window of {} (±{}); re-centred on the mid when a best comes within {margin} levels of an edge:",
             fmt_count(walk.len() as u64), fmt_count((last - first).unsigned_abs()), fmt_count(LEVELS as u64), fmt_count(LEVELS as u64 / 2));
    for (name, h) in [("copy re-base", &hc), ("ring re-base", &hr)] {
        let s = summarize(h, 1e3, clock.pair_ns());
        println!("    {name:<13} {} re-bases: p50 {} p99 {} max {} µs", fmt_count(s.n), fmt_ns(s.p50), fmt_ns(s.p99), fmt_ns(s.max));
    }
    let final_levels = venue.snapshot();
    println!("    updates after which the top differed from the venue's: plain array {}, copy re-base {}, ring {}",
             fmt_count(wrong[0]), fmt_count(wrong[1]), fmt_count(wrong[2]));
    println!("    venue levels missing from the final window (holes from levels that entered unseen): copy {}, ring {}",
             holes(&final_levels, copy.base(), |s, p| copy.qty_at(s, p)), holes(&final_levels, ring.base(), |s, p| ring.qty_at(s, p)));
}

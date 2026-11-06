//! build: exercise 1. `TreeBook`, `VecBook` and `ArrayBook` behind the shared
//! `Book` trait, the array with a 65,536-level window centred on the first
//! mid, all allocated in `new`; the property check that feeds all three the
//! same random updates and compares their tops after every update, not only
//! at the end, with zero quantities common enough that the best empties and
//! must be found again. The library's tests run the same check
//! (`cargo test -p ch06-book`); this runs it at any size and times each apply.

use ch05_structures::lab::{self, count, fmt_count, new_hist, now, summarize, Clock};
use ch06_book::{gen, ArrayBook, Book, Side, Ticks, TreeBook, VecBook, LEVELS};
use std::process::exit;

const HELP: &str = "\
book-build: exercise 1 (the chapter's `build`; cargo reserves that name), three books,
one trait, identical tops after every update

usage: book-build [--records N] [--seed S] [--zero-one-in K] [--band B] [--core N|none]

  --records N    random updates (default 1,000,000)
  --seed S       their seed (default 7)
  --zero-one-in K  one update in K removes its level (default 3)
  --band B       levels each side of the mid the updates land on (default 200, so
                 the walks inward cross 64-bit words of the bitmap)
  --core N       pin to core N (default 3); `none` for unpinned

Exits 1 at the first update after which the tops differ, printing it.";

const MID: Ticks = 6_812_345;

fn main() {
    let (mut records, mut seed, mut zero, mut band, mut core) = (1_000_000u64, 7u64, 3u64, 200i64, Some(3));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let n = |args: &mut std::iter::Skip<std::env::Args>| count(&lab::value(args, &a, HELP)).unwrap_or_else(|| lab::usage(&format!("{a}: a count"), HELP));
        match a.as_str() {
            "--records" => records = n(&mut args),
            "--seed" => seed = n(&mut args),
            "--zero-one-in" => zero = n(&mut args).max(1),
            "--band" => band = n(&mut args).clamp(1, 30_000) as i64,
            "--core" => core = lab::core(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--core: a core or none", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }

    lab::predict(&[
        "write down which structure you expect to fail first, and on which kind of update.",
        "The chapter's answer: the walk inward, when the best level empties and has to be found",
        "again, is where the three disagree first; the array's cursor and bitmap are the code at risk.",
    ]);

    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("{}", lab::pin_or_warn("build", core));
    let updates = gen::random(records as usize, seed, MID, band, zero);
    let mid = gen::first_mid(&updates).unwrap_or(MID);
    let base = mid - (LEVELS / 2) as Ticks;
    println!("{} updates, seed {seed}, one in {zero} a removal, {band} levels a side; the array's window \
              {base}..{} centred on the first mid {mid}\n", fmt_count(records), base + LEVELS as Ticks);

    let (mut array, mut vec, mut tree) = (ArrayBook::new(base), VecBook::with_capacity(4 * band as usize), TreeBook::default());
    let mut hist = [new_hist(), new_hist(), new_hist()];
    let (mut changes, mut emptied) = (0u64, 0u64);
    for u in &updates {
        let before = tree.top();
        let t0 = now(); let ca = array.apply(u); let t1 = now();
        let cv = vec.apply(u); let t2 = now();
        let ct = tree.apply(u); let t3 = now();
        for (h, d) in hist.iter_mut().zip([t1 - t0, t2 - t1, t3 - t2]) { h.saturating_record(clock.ns(d)); }
        let best = match u.side { Side::Bid => before.bid, Side::Ask => before.ask };
        emptied += (u.qty == 0 && best.is_some_and(|(p, _)| p == u.price)) as u64;   // the walk inward
        changes += ct as u64;
        if (ca, cv) != (ct, ct) || array.top() != tree.top() || vec.top() != tree.top() {
            println!("DISAGREE after seq {} ({:?} {} qty {}):", u.seq, u.side, u.price, u.qty);
            println!("  apply returned: array {ca}, vec {cv}, tree {ct}");
            println!("  tree  (the reference) {:?}\n  vec   {:?}\n  array {:?}", tree.top(), vec.top(), array.top());
            println!("  the best level was {:?} before it", best);
            exit(1);
        }
    }
    println!("the three tops agreed after every one of {} updates: {} top changes, {} times the best emptied \
              and was found again\n", fmt_count(records), fmt_count(changes), fmt_count(emptied));
    println!("apply, each timed alone, minus the clock pair (the replay lab times them properly):");
    for (name, h) in ["ArrayBook", "VecBook", "TreeBook"].iter().zip(&hist) {
        println!("  {name:<10} {}", summarize(h, 1.0, clock.pair_ns()).line());
    }
}

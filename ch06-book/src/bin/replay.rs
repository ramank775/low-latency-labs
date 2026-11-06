//! replay: exercise 2. The same stream of 10 million updates through each
//! structure: updates per second from an untimed pass, then the p50, p99 and
//! max of `apply` from a timed pass, each call between two fenced `rdtsc`
//! reads with the cost of two reads back to back subtracted, sorted by what
//! the update did to the top. The stream is the generator's (95% deep, 4% the
//! best level's quantity, 1% moving the best price), or a file of 56-byte
//! records such as the capstone's `gen` writes or `--write` saves.

use ch05_structures::lab::{self, count, fmt_count, summarize, Clock, Kernel};
use ch06_book::{gen, timed_replay, untimed_replay, ArrayBook, Book, Ticks, TreeBook, VecBook, LEVELS};
use std::path::PathBuf;

const HELP: &str = "\
book-replay: exercise 2 (the chapter's `replay`; ch04-network has that name), updates per
second and the p50, p99 and max of apply

usage: book-replay [all|array|vec|tree] [FILE] [--records N] [--seed S] [--write FILE]
                   [--warmup N] [--core N|none]

  all|array|vec|tree  which structures (default all, in that order)
  FILE           replay this file of 56-byte records (an arrival time, then the
                 networking chapter's 48-byte message) instead of generating
  --records N    updates (default 10,000,000; with FILE, at most N of it)
  --seed S       the generator's seed (default 7)
  --write FILE   save the generated stream as 56-byte records first, so that perf
                 can replay the same file: book-replay --write updates-10m.bin
  --warmup N     updates applied before any is recorded, so a fresh array's first
                 touch of each page is not in the numbers (default 100,000)
  --core N       pin to core N (default 3); `none` for unpinned

The chapter counts the same replay under perf (needs perf):
  perf stat -e cycles,instructions,branch-misses,cache-misses,L1-dcache-load-misses \\
      -- taskset -c 3 ./book-replay array updates-10m.bin
  perf record -g -- taskset -c 3 ./book-replay tree updates-10m.bin; perf report
and divides by 10,000,000. cargo asm --lib \"<ch06_book::ArrayBook as ch06_book::Book>::apply\"";

fn main() {
    let (mut which, mut file, mut records, mut seed, mut write, mut warmup, mut core) =
        (String::from("all"), None::<PathBuf>, 10_000_000u64, 7u64, None::<PathBuf>, 100_000u64, Some(3));
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "all" | "array" | "vec" | "tree" => which = a,
            "--records" => records = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--records: a count", HELP)),
            "--seed" => seed = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--seed: an integer", HELP)),
            "--write" => write = Some(PathBuf::from(lab::value(&mut args, &a, HELP))),
            "--warmup" => warmup = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--warmup: a count", HELP)),
            "--core" => core = lab::core(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--core: a core or none", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            f if !f.starts_with('-') && file.is_none() => file = Some(PathBuf::from(f)),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }

    lab::predict(&[
        "the ranking ArrayBook, VecBook, TreeBook. ArrayBook at 2-5 ns, its p99 where the 1% that move",
        "the cursor land, 5-20 ns; VecBook 20-60 ns to find a level, 120-360 ns to add or remove one;",
        "TreeBook, its two best-price walks included, 160-500 ns on an existing level and 210-700 ns on",
        "a new or emptied one. An ArrayBook max far above its p99 is the machine, not the structure.",
    ]);

    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("{}", lab::pin_or_warn("replay", core));

    let updates = match &file {
        Some(f) => gen::read_records(f, records as usize).unwrap_or_else(|e| lab::usage(&format!("{}: {e}", f.display()), HELP)),
        None => {
            let (v, m) = gen::generate(&gen::Params { records, seed, ..Default::default() });
            let n = (v.len() as u64 - m.opening).max(1) as f64;
            println!("generated {} updates, seed {seed}: {} opening the book, then deep {:.1}%, best quantity {:.1}%, \
                      best price moves {:.1}%; prices {}..{}", fmt_count(v.len() as u64), m.opening,
                     100.0 * m.deep as f64 / n, 100.0 * m.best_qty as f64 / n, 100.0 * m.moves as f64 / n, m.min_price, m.max_price);
            v
        }
    };
    if let Some(w) = &write {
        gen::write_records(w, &updates, 50_000).unwrap_or_else(|e| lab::usage(&format!("{}: {e}", w.display()), HELP));
        println!("wrote {} records of 56 bytes to {}", fmt_count(updates.len() as u64), w.display());
    }
    if updates.is_empty() { lab::usage("no depth updates to replay", HELP); }
    let mid = gen::first_mid(&updates).unwrap_or(updates[0].price);
    let base = mid - (LEVELS / 2) as Ticks;
    let warmup = (warmup as usize).min(updates.len() / 2);
    println!("{} updates; ArrayBook's window centred on the first mid {mid}; the first {} applied unrecorded\n",
             fmt_count(updates.len() as u64), fmt_count(warmup as u64));

    let books: Vec<&str> = if which == "all" { vec!["array", "vec", "tree"] } else { vec![which.as_str()] };
    let mut tops = Vec::new();
    for b in books {
        // two fresh books: one untimed for the rate, one timed for the percentiles
        let (rate, changes, timed, k, top) = match b {
            "array" => run(|| ArrayBook::new(base), &updates, warmup, &clock),
            "vec" => run(|| VecBook::with_capacity(LEVELS), &updates, warmup, &clock),
            _ => run(TreeBook::default, &updates, warmup, &clock),
        };
        let name = match b { "array" => "ArrayBook", "vec" => "VecBook (preallocated)", _ => "TreeBook" };
        println!("{name}: {:.1} M updates/s untimed; top changed {} per 1,000", rate / 1e6, changes * 1_000 / updates.len() as u64);
        let p = clock.pair_ns();
        println!("  apply, all       {}", summarize(&timed.all, 1.0, p).line());
        println!("    top unchanged  {}", summarize(&timed.unchanged, 1.0, p).line());
        println!("    best quantity  {}", summarize(&timed.best_qty, 1.0, p).line());
        println!("    best moved     {}", summarize(&timed.moved, 1.0, p).line());
        println!("  during the timed pass: {}\n", k.map_or("no /proc counts".to_string(), |k| k.line()));
        tops.push((name, top));
    }
    if tops.windows(2).all(|w| w[0].1 == w[1].1) && tops.len() > 1 {
        println!("final tops agree: {:?}", tops[0].1);
    } else if tops.len() > 1 {
        println!("FINAL TOPS DIFFER: {tops:?}");
    }
}

type Outcome = (f64, u64, ch06_book::Timed, Option<Kernel>, ch06_book::Top);

fn run<B: Book>(make: impl Fn() -> B, updates: &[ch06_book::Update], warmup: usize, clock: &Clock) -> Outcome {
    let mut fresh = make();
    let (rate, changes) = untimed_replay(&mut fresh, updates);
    let mut book = make();
    let k0 = Kernel::read();
    let timed = timed_replay(&mut book, updates, warmup, clock);
    let k = Kernel::read().zip(k0).map(|(a, b)| a.since(&b));
    (rate, changes, timed, k, book.top())
}

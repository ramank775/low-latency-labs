//! alloc: exercise 4. The ring benchmark under the counting global allocator:
//! the consumer reads the count when the warm-up ends, both threads running,
//! and again at its last item, and the two must be equal. Then the same for
//! `std::sync::mpsc::channel()`, which fails by one block for every 31 items,
//! and for `sync_channel(4096)`, whose buffer is allocated once.

use ch05_structures::bench::{self, Cfg, Queue, BLOCK};
use ch05_structures::counting::{self, Counting};
use ch05_structures::lab::{self, count, fmt_count, summarize, Clock};
use std::process::exit;

#[global_allocator]
static GLOBAL: Counting = Counting;

const HELP: &str = "\
alloc: exercise 4, the ring benchmark under the counting global allocator

usage: alloc [all|ring|channel|sync] [--items N] [--warmup N] [--cores A,B|none]

  ring           the SPSC ring: the count after warm-up must equal the count at the end
  channel        std::sync::mpsc::channel(): an allocation per 31 items, by design
  sync           std::sync::mpsc::sync_channel(4096): predict it first
  all            the three in turn (default)
  --items N      items through each queue (default 100,000,000)
  --warmup N     items before the count starts (default 1,000,000, at most half the run)
  --cores A,B    producer and consumer cores (default 2,3); `none` for unpinned

The histogram is made before the start and the threads are spawned before the
count starts: the counter sees every thread. Exits 1 if the ring allocated.";

fn main() {
    let (mut which, mut items, mut warmup, mut cores) = (String::from("all"), 100_000_000u64, 1_000_000u64, [Some(2), Some(3)]);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "all" | "ring" | "channel" | "sync" => which = a,
            "--items" => items = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--items: a count", HELP)),
            "--warmup" => warmup = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--warmup: a count", HELP)),
            "--cores" => cores = lab::cores(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--cores: A,B or none", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }
    if items < 2 * BLOCK { lab::usage("--items must be at least 8,192", HELP); }
    let warmup = warmup.min(items / 2).max(1);

    lab::predict(&[
        "the ring allocates nothing after warm-up: the two counts are equal.",
        "channel() allocates a block for every 31 items, about 3.2 million over 100 million.",
        "sync_channel(4096), whose buffer is allocated once: write down whether it passes.",
    ]);

    if !counting::installed() { eprintln!("alloc: the counting allocator is not installed"); exit(2); }
    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{} items a queue; the count starts after {} items, both threads running\n", fmt_count(items), fmt_count(warmup));

    let queues = match which.as_str() {
        "ring" => vec![Queue::Spsc],
        "channel" => vec![Queue::Mpsc],
        "sync" => vec![Queue::Sync],
        _ => vec![Queue::Spsc, Queue::Mpsc, Queue::Sync],
    };
    let mut ring_failed = false;
    for q in queues {
        let cfg = Cfg { items, warmup, cores, clock, period_ns: 0 };
        let r = bench::run(q, &cfg, false);
        let measured = r.measured;
        let held = r.allocs == 0 && r.frees == 0;
        println!("{}", q.name());
        for p in &r.pins { println!("  {p}"); }
        println!("  {} items after warm-up, {:.1} M items/s", fmt_count(measured), r.items_per_sec() / 1e6);
        println!("    per item, by block of {BLOCK}: {}", summarize(&r.hist, BLOCK as f64, clock.pair_ns()).line());
        println!("  between warm-up and the last item: {} allocations, {} frees{}", fmt_count(r.allocs), fmt_count(r.frees),
                 if r.allocs == 0 { String::new() } else { format!(", one allocation per {:.1} items", measured as f64 / r.allocs as f64) });
        println!("  assert_eq!(count after warm-up, count at the end): {}\n",
                 if held { "holds".to_string() } else { format!("FAILS: {} allocations and {} frees", fmt_count(r.allocs), fmt_count(r.frees)) });
        if q == Queue::Spsc && !held { ring_failed = true; }
    }
    if ring_failed { eprintln!("alloc: the ring allocated after warm-up"); exit(1); }
}

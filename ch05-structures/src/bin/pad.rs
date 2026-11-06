//! pad: exercise 2. The same ring with `head` and `tail` moved onto one cache
//! line, cached copies kept, measured beside the padded ring: whatever changes
//! is false sharing. The producer's store to `head` takes the line from the
//! consumer's core and the consumer's store to `tail` takes it back; the
//! cached copies save reads of the other side's counter, not writes to your own line.

use ch05_structures::bench::{self, Cfg, Queue, BLOCK};
use ch05_structures::lab::{self, count, fmt_count, fmt_ns, summarize, Clock, Summary};

const HELP: &str = "\
pad: exercise 2, head and tail on one cache line against the padded ring

usage: pad [all|padded|shared] [--items N] [--paced-items N] [--period-ns NS]
           [--warmup N] [--cores A,B|none]

  all|padded|shared  which layout (default all: both, padded first)
  --items N      items through each ring flat out (default 100,000,000)
  --paced-items N  items in the paced latency run (default 10,000,000)
  --period-ns NS the paced producer's period (default 1000)
  --warmup N     items left out of every histogram (default 1% of each run)
  --cores A,B    producer and consumer cores (default 2,3); `none` for unpinned

Then explain the result with Chapter 1's false-sharing lab. With perf (root or
kernel.perf_event_paranoid <= 0):
  perf c2c record -- ./pad shared --items 10000000; perf c2c report
should put the counters' line first by HITM events, from both cores, at two
offsets eight bytes apart.";

fn main() {
    let (mut which, mut items, mut paced, mut period, mut warmup, mut cores) =
        (String::from("all"), 100_000_000u64, 10_000_000u64, 1_000u64, None, [Some(2), Some(3)]);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "all" | "padded" | "shared" => which = a,
            "--items" => items = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--items: a count", HELP)),
            "--paced-items" => paced = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--paced-items: a count", HELP)),
            "--period-ns" => period = count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--period-ns: nanoseconds", HELP)),
            "--warmup" => warmup = Some(count(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--warmup: a count", HELP))),
            "--cores" => cores = lab::cores(&lab::value(&mut args, &a, HELP)).unwrap_or_else(|| lab::usage("--cores: A,B or none", HELP)),
            "-h" | "--help" => lab::usage("", HELP),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }
    if items < 2 * BLOCK || paced < 100 { lab::usage("--items must be at least 8,192 and --paced-items at least 100", HELP); }

    lab::predict(&[
        "the padded ring at 5-20 ns an item; with head and tail on one line it falls to 30-100 ns,",
        "since every push and every pop must take the line back from the other core (50-100 ns),",
        "and perf c2c ranks that line first by HITM events.",
    ]);

    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("flat out: {} items; paced: {} items, one every {} ns\n", fmt_count(items), fmt_count(paced), period);

    let rings = match which.as_str() {
        "padded" => vec![Queue::Spsc],
        "shared" => vec![Queue::SharedLine],
        _ => vec![Queue::Spsc, Queue::SharedLine],
    };
    let mut rows: Vec<(Queue, Summary, Summary)> = Vec::new();
    for q in rings {
        println!("{}", q.name());
        let flat = Cfg { items, warmup: warmup.unwrap_or(items / 100), cores, clock, period_ns: 0 };
        let r = bench::run(q, &flat, false);
        for p in &r.pins { println!("  {p}"); }
        let t = summarize(&r.hist, BLOCK as f64, clock.pair_ns());
        println!("  throughput, flat out: {:.1} M items/s", r.items_per_sec() / 1e6);
        println!("    per item, by block of {BLOCK}: {}", t.line());
        if r.out_of_order != 0 { println!("    OUT OF ORDER: {} items", r.out_of_order); }
        let pcfg = Cfg { items: paced, warmup: warmup.unwrap_or(paced / 100).min(paced / 2), cores, clock, period_ns: period };
        let r = bench::run(q, &pcfg, true);
        let l = summarize(&r.hist, 1.0, clock.pair_ns());
        println!("  latency, paced one per {period} ns, push to pop, minus the clock pair:");
        println!("    {}\n", l.line());
        rows.push((q, t, l));
    }
    if let [(_, pt, pl), (_, st, sl)] = rows.as_slice() {
        println!("one line against padded, p50 to p50: throughput per item {}x, paced latency {}x",
                 fmt_ns(st.p50 / pt.p50.max(1e-9)), fmt_ns(sl.p50 / pl.p50.max(1e-9)));
    }
}

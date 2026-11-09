//! ring: exercise 1. The SPSC ring with `u64` slots, capacity 4,096, cached
//! copies of the other side's counter, against `std::sync::mpsc` (the consumer
//! spinning on `try_recv`) and a `Mutex<VecDeque<u64>>`: throughput flat out,
//! then per-item latency with the producer paced to one item a microsecond,
//! its `rdtsc` stamped into the `u64` and subtracted on pop; then flat out
//! with stamps, where a full ring makes latency only queueing.

use ch05_structures::bench::{self, Cfg, Queue, BLOCK};
use ch05_structures::lab::{self, count, fmt_count, fmt_ns, summarize, Clock, Summary};

const HELP: &str = "\
ring: exercise 1, the SPSC ring against std::sync::mpsc and Mutex<VecDeque<u64>>

usage: ring [all|spsc|mpsc|mutex] [--items N] [--paced-items N] [--period-ns NS]
            [--warmup N] [--cores A,B|none]

  all|spsc|...   which queues (default all)
  --items N      items through each queue flat out (default 100,000,000)
  --paced-items N  items in the paced latency run (default 100,000,000: 100 s a
                 queue at one a microsecond)
  --period-ns NS the paced producer's period (default 1000: one item a microsecond)
  --warmup N     items left out of every histogram (default 1% of each run)
  --cores A,B    producer and consumer cores (default 2,3; check in lscpu -e that
                 they share an L3); `none` runs both unpinned

Pin the whole run too, as the chapter does: taskset -c 2,3 ./ring
perf: perf c2c record -- ./ring spsc; perf c2c report (needs perf, and root or
kernel.perf_event_paranoid <= 0) ranks the lines that moved between cores.";

fn main() {
    let (mut which, mut items, mut paced, mut period, mut warmup, mut cores) =
        (String::from("all"), 100_000_000u64, 100_000_000u64, 1_000u64, None, [Some(2), Some(3)]);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "all" | "spsc" | "mpsc" | "mutex" => which = a,
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
        "the ring at 5-20 ns an item; mpsc at 100-300 ns with the consumer spinning on try_recv;",
        "the mutex at 150-400 ns: each roughly 5-80 times the ring, 15-25 at the middles of the ranges,",
        "and within a factor of two of the other.",
        "Paced at one item a microsecond, latency is the hand-off itself; flat out against a full",
        "ring it is only queueing: about 4,096 items' worth of the consumer's time.",
    ]);

    let clock = Clock::new();
    println!("{}", lab::machine());
    println!("{}", clock.describe());
    println!("flat out: {} items a queue; paced: {} items, one every {} ns\n", fmt_count(items), fmt_count(paced), period);

    let queues: Vec<Queue> = match which.as_str() {
        "spsc" => vec![Queue::Spsc],
        "mpsc" => vec![Queue::Mpsc],
        "mutex" => vec![Queue::Mutex],
        _ => vec![Queue::Spsc, Queue::Mpsc, Queue::Mutex],
    };
    let mut rows: Vec<(Queue, Summary, Summary)> = Vec::new();
    for q in queues {
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
        println!("    {}", l.line());

        let fcfg = Cfg { items: items.min(10_000_000), warmup: warmup.unwrap_or(items.min(10_000_000) / 100), cores, clock, period_ns: 0 };
        let r = bench::run(q, &fcfg, true);
        let f = summarize(&r.hist, 1.0, clock.pair_ns());
        println!("  latency, flat out with stamps (the queue full): queueing, not the hand-off:");
        println!("    {}", f.line());
        if r.backwards != 0 { println!("    {} pops read a clock below their stamp: two cores' TSCs disagree", r.backwards); }
        println!();
        rows.push((q, t, l));
    }

    if let Some(&(_, ring_t, ring_l)) = rows.iter().find(|r| r.0 == Queue::Spsc) {
        println!("against the ring (p50 to p50; the chapter predicts roughly 5-80x flat out, 1.5-3x paced):");
        for (q, t, l) in rows.iter().filter(|r| r.0 != Queue::Spsc) {
            println!("  {:<24} throughput per item {:>6}x   paced latency {:>6}x", q.name(),
                     fmt_ns(t.p50 / ring_t.p50.max(1e-9)), fmt_ns(l.p50 / ring_l.p50.max(1e-9)));
        }
    }
}

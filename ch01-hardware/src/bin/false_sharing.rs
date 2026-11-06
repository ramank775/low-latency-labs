//! false_sharing: Lab 1.3, false sharing.
//!
//!     false_sharing [--variant adjacent|padded|align64|all] [--iters N] [--block N] [--cores A,B]
//!
//! Starts two threads pinned to chosen cores; each performs 100 million
//! `fetch_add(1, Ordering::Relaxed)` on its own `AtomicU64`. Adjacent: the
//! counters side by side in one struct. Padded: each wrapped in `CachePadded`.
//! align64: the chapter's own `Counters`, each in a `#[repr(align(64))]` `Padded`.

use ch01_hardware::{calibrate, commas, machine_line, package, predict, run_pair, thread_siblings, Adjacent,
                    Cli, CoreArg, Counters, Hammered, PaddedPair, Summary};
use std::sync::atomic::{AtomicU64, Ordering};

const VARIANTS: [(&str, &str); 3] = [
    ("adjacent", "a at byte 0, b at byte 8: one line"),
    ("padded", "CachePadded: 128 bytes apart"),
    ("align64", "Counters, Padded<T>: 64 bytes apart"),
];

fn help() -> String {
    "false_sharing: Lab 1.3, false sharing.

usage: false_sharing [--variant NAME] [--iters N] [--block N] [--cores A,B]

  --variant NAME  adjacent, padded, align64 or all (default all)
  --iters N       increments per thread (default 100,000,000)
  --block N       increments per timed block: each block is one sample (default 1,000,000)
  --cores A,B     the two threads' cores (default 2,3); `none` for an unpinned thread,
                  e.g. --cores none,none. A refused pin is reported and the run goes on.

variants:
  adjacent   struct Adjacent { a: AtomicU64, b: AtomicU64 }, 64-aligned: a at 0, b at 8, one line
  padded     each counter in crossbeam's CachePadded (128 bytes on x86-64), as the lab says
  align64    the chapter's Counters: each counter in Padded<T>, #[repr(align(64))]

Pick two cores that are not hyperthread siblings (they share one L1 and the line
never moves): cat /sys/devices/system/cpu/cpu3/topology/thread_siblings_list
Cores on different sockets make the adjacent case far worse; see it once.

Find the line with perf (needs the perf tool and usually root):
  perf c2c record -- ./target/release/false_sharing --variant adjacent --cores 2,3
  perf c2c report        # the hot line shows offsets 0x0 and 0x8, one per thread".to_string()
}

fn main() {
    let mut cli = Cli::new("false_sharing", help());
    let (mut variant, mut iters, mut block) = ("all".to_string(), 100_000_000u64, 1_000_000u64);
    let mut cores = [CoreArg::Core(2), CoreArg::Core(3)];
    while let Some(f) = cli.flag() {
        match f.as_str() {
            "--variant" => variant = cli.value("--variant"),
            "--iters" => iters = cli.count("--iters"),
            "--block" => block = cli.count("--block"),
            "--cores" => {
                let v = cli.value("--cores");
                let parts: Vec<&str> = v.split(',').collect();
                if parts.len() != 2 { cli.fail("--cores: two cores, A,B (or none,none)"); }
                cores = [cli.parse_core("--cores", parts[0]), cli.parse_core("--cores", parts[1])];
            }
            other => cli.fail(&format!("unknown flag {other}")),
        }
    }
    let chosen: Vec<&str> = if variant == "all" {
        VARIANTS.iter().map(|v| v.0).collect()
    } else if let Some(v) = VARIANTS.iter().find(|x| x.0 == variant) {
        vec![v.0]
    } else {
        cli.fail(&format!("--variant {variant}: adjacent, padded, align64 or all"))
    };

    let scale = if iters == 100_000_000 { String::new() } else {
        format!(" (at --iters {}: {:.3} s padded, {:.2}-{:.1} s adjacent)", commas(iters),
                iters as f64 * 5e-9, iters as f64 * 25e-9, iters as f64 * 100e-9)
    };
    predict(&format!(
        "padded, each line stays Modified in its own L1 and an atomic add costs about 5 ns: \
         100 M x 5 ns = 0.5 s. Adjacent, the line bounces; not every increment pays the full \
         50-100 ns, but many do: 5-20x slower, roughly 2.5-10 s{scale}. Write down the ratio you expect."));

    let tpn = calibrate();
    println!("{}", machine_line(tpn));
    if let [CoreArg::Core(a), CoreArg::Core(b)] = cores {
        if a == b {
            println!("warning: both threads on core {a}: they take turns, the line never moves, the effect vanishes");
        } else if thread_siblings(a).is_some_and(|s| s.contains(&b)) {
            println!("warning: cores {a} and {b} are hyperthread siblings: one L1, so the line never moves");
        }
        match (package(a), package(b)) {
            (Some(pa), Some(pb)) if pa != pb => println!("cores {a} and {b} are on different sockets ({pa}, {pb}): expect the adjacent case far worse"),
            (Some(pa), Some(_)) => println!("cores {a} and {b}: one socket ({pa}), not hyperthread siblings unless warned above"),
            _ => {}
        }
    }
    println!("{} increments per thread, timed in blocks of {}: one sample per block per thread",
             commas(iters), commas(block));
    println!();

    let mut walls = Vec::new();
    for &name in &chosen {
        let (adjacent, padded, counters) = (Adjacent::default(), PaddedPair::default(), Counters::default());
        let (a, b): (&AtomicU64, &AtomicU64) = match name {
            "adjacent" => (&adjacent.a, &adjacent.b),
            "padded" => (&*padded.a, &*padded.b),
            _ => (&counters.a.0, &counters.b.0),
        };
        let gap = (b as *const AtomicU64 as usize).abs_diff(a as *const AtomicU64 as usize);
        let line_a = a as *const AtomicU64 as usize / 64;
        let line_b = b as *const AtomicU64 as usize / 64;
        let t = run_pair(a, b, cores, iters, block);
        if walls.is_empty() {
            for (name, h) in ["a", "b"].iter().zip(&t) {
                if h.pinned.refused { eprintln!("warning: thread {name} {}", h.pinned.line); }
                println!("thread {name} {}", h.pinned.line);
            }
            println!();
            println!(" variant    layout                                 wall (s)    time per increment (ns)       samples");
            println!("                                                                 p50      p99      max");
        }
        let (got_a, got_b) = (a.load(Ordering::Relaxed), b.load(Ordering::Relaxed));
        if got_a != iters || got_b != iters {
            println!("ERROR: counts {got_a} and {got_b}, expected {iters} each");
            std::process::exit(1);
        }
        let wall = report(name, &t, tpn, gap, line_a == line_b);
        walls.push((name, wall));
    }
    println!();
    if let Some(&(_, adj)) = walls.iter().find(|w| w.0 == "adjacent") {
        for &(name, w) in walls.iter().filter(|w| w.0 != "adjacent") {
            println!("adjacent / {name}: {:.1}x in wall time (the chapter: 5-20x)", adj / w);
        }
    }
    println!("Write down both times, the ratio, and the cores used.");
}

/// One row: wall time over both threads, and the per-increment time of every block.
fn report(name: &str, t: &[Hammered; 2], tpn: f64, gap: usize, same_line: bool) -> f64 {
    let start = t[0].start.min(t[1].start);
    let end = t[0].end.max(t[1].end);
    let overlap = t[0].end.min(t[1].end).saturating_sub(t[0].start.max(t[1].start));
    let wall = (end - start) as f64 / tpn / 1e9;
    let mut per: Vec<f64> = t.iter().flat_map(|h| h.blocks.iter().map(|&(n, ticks)| ticks as f64 / tpn / n as f64)).collect();
    let p = Summary::of(&mut per);
    let what = VARIANTS.iter().find(|v| v.0 == name).map_or("", |v| v.1);
    println!(" {name:<10} {what:<38} {wall:>8.3}   {:>8.2} {:>8.2} {:>8.2}   {:>7}", p.p50, p.p99, p.max, p.n);
    println!("            ({gap} bytes apart, {}; the threads overlapped for {:.0}% of the wall time)",
             if same_line { "same line" } else { "different lines" }, 100.0 * overlap as f64 / (end - start) as f64);
    wall
}

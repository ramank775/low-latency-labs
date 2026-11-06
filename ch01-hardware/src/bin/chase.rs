//! chase: Lab 1.4 (stretch), pointer chasing.
//!
//!     chase [--nodes N] [--variant vec|list|boxes|all] [--reps N] [--no-shuffle] [--huge]
//!           [--core N|none] [--seed S]
//!
//! Builds 10 million nodes two ways, a `Vec<u64>` summed in order and a
//! linked list of individually heap-allocated nodes whose links are shuffled
//! into random order, and times a full traversal of each. `boxes` adds
//! section 1.4's case in between, `Vec<Box<u64>>`.

use ch01_hardware::{anon_huge_kb, boxes, boxes_sum, calibrate, commas, list_sum, machine_line, now, permutation,
                    pin_line, predict, vec_sum, Buffer, Cli, CoreArg, List, Summary};
use std::hint::black_box;

const VARIANTS: [(&str, &str); 3] = [
    ("vec", "Vec<u64>, in order"),
    ("list", "linked list, one Box per node"),
    ("boxes", "Vec<Box<u64>>"),
];

fn help() -> String {
    "chase: Lab 1.4 (stretch), pointer chasing.

usage: chase [--nodes N] [--variant NAME] [--reps N] [--no-shuffle] [--huge] [--core N|none] [--seed S]

  --nodes N       elements in each structure (default 10,000,000)
  --variant NAME  vec, list, boxes or all (default all)
  --reps N        timed traversals per variant, after one warm-up traversal (default 5)
  --no-shuffle    link (and place) the nodes in allocation order: the list then looks
                  only a few times slower, the trap of section 1.4
  --huge          ask for 2 MB pages for the Vec<u64> (madvise; no root while THP is
                  `madvise` or `always`). The list's and the boxes' nodes come from
                  malloc: for those, run with GLIBC_TUNABLES=glibc.malloc.hugetlb=1
                  (glibc 2.35 or later), or set THP to `always` (root)
  --core N        pin to core N (default 3, or the single CPU `taskset -c` started it on);
                  `none` runs unpinned. A refused pin is reported and the run goes on.
  --seed S        the shuffle's seed (default 7)

With and without huge pages, as the lab asks:
  ./target/release/chase
  GLIBC_TUNABLES=glibc.malloc.hugetlb=1 ./target/release/chase --huge
TLB misses, with perf: perf stat -e dTLB-load-misses,cycles ./target/release/chase --variant list".to_string()
}

fn main() {
    let mut cli = Cli::new("chase", help());
    let (mut nodes, mut variant, mut reps, mut shuffle, mut huge, mut core, mut seed) =
        (10_000_000u64, "all".to_string(), 5u64, true, false, CoreArg::Default(3), 7u64);
    while let Some(f) = cli.flag() {
        match f.as_str() {
            "--nodes" => nodes = cli.count("--nodes"),
            "--variant" => variant = cli.value("--variant"),
            "--reps" => reps = cli.count("--reps"),
            "--no-shuffle" => shuffle = false,
            "--huge" => huge = true,
            "--core" => core = cli.core("--core"),
            "--seed" => seed = cli.value("--seed").parse().unwrap_or_else(|_| cli.fail("--seed: an integer")),
            other => cli.fail(&format!("unknown flag {other}")),
        }
    }
    if nodes > u32::MAX as u64 { cli.fail("--nodes: at most 4,294,967,295"); }
    let chosen: Vec<&str> = if variant == "all" {
        VARIANTS.iter().map(|v| v.0).collect()
    } else if let Some(v) = VARIANTS.iter().find(|x| x.0 == variant) {
        vec![v.0]
    } else {
        cli.fail(&format!("--variant {variant}: vec, list, boxes or all"))
    };
    let n = nodes as usize;

    let scale = if n == 10_000_000 { String::new() } else {
        format!(" (at --nodes {}: Vec {:.2}-{:.2} ms, list {:.0}-{:.0} ms)", commas(nodes),
                nodes as f64 * 0.5e-6, nodes as f64 * 1e-6, nodes as f64 * 100e-6, nodes as f64 * 150e-6)
    };
    predict(&format!(
        "the Vec is 80 MB read in order, bound by bandwidth: about 5-10 ms, under 1 ns per element. \
         The list is a chain of dependent loads to random addresses, each a DRAM miss plus usually a \
         TLB miss: 100-150 ns per node, 1-1.5 s in total, 100-300x the Vec{scale}. Vec<Box<u64>> \
         (section 1.4) in between: 10-40x the Vec. Without the shuffle the list looks only a few times slower."));

    let pinned = pin_line(core); // before building: the nodes' pages are placed by this thread's first touch
    let tpn = calibrate();
    println!("{}", machine_line(tpn));
    println!("{pinned}");
    let tunables = std::env::var("GLIBC_TUNABLES").unwrap_or_else(|_| "unset".to_string());
    println!("{} nodes, links {}; seed {seed}; Vec<u64> pages: {}; malloc'd nodes: GLIBC_TUNABLES {tunables}",
             commas(nodes), if shuffle { "shuffled into random order" } else { "in allocation order (--no-shuffle)" },
             if huge { "2 MB asked for (madvise)" } else { "4 KB" });
    println!("{reps} timed traversals per variant after one warm-up");
    println!();
    println!(" variant  what                             time per traversal (ms)          time per element (ns)     AnonHuge kB,");
    println!("                                           p50       p99       max         p50     p99     max     n  process");

    let order = permutation(n, shuffle, seed);
    let expect = nodes * (nodes - 1) / 2; // 0 + 1 + ... + (n - 1)
    let mut p50 = Vec::new();
    for &name in &chosen {
        let what = VARIANTS.iter().find(|v| v.0 == name).map_or("", |v| v.1);
        let (p, huge_kb, sum) = match name {
            "vec" => {
                let mut v = Buffer::new(n, 0u64, huge);
                for (i, x) in v.iter_mut().enumerate() { *x = i as u64; }
                let kb = anon_huge_kb();
                let (p, s) = time(reps, tpn, || vec_sum(black_box(&v)));
                (p, kb, s)
            }
            "list" => {
                let list = List::build(&order);
                let kb = anon_huge_kb();
                let (p, s) = time(reps, tpn, || list_sum(black_box(&list)));
                (p, kb, s) // the list is dropped here, one node at a time, untimed
            }
            _ => {
                let b = boxes(&order);
                let kb = anon_huge_kb();
                let (p, s) = time(reps, tpn, || boxes_sum(black_box(&b)));
                (p, kb, s)
            }
        };
        if sum != expect {
            println!("ERROR: {name} summed to {sum}, expected {expect}");
            std::process::exit(1);
        }
        let per = |t: f64| t / n as f64;
        println!(" {name:<8} {what:<30} {:>9.3} {:>9.3} {:>9.3}   {:>9.2} {:>7.2} {:>7.2} {:>5} {:>8}",
                 p.p50 / 1e6, p.p99 / 1e6, p.max / 1e6, per(p.p50), per(p.p99), per(p.max), p.n,
                 huge_kb.map_or("?".to_string(), commas));
        p50.push((name, p.p50));
    }
    println!();
    if let Some(&(_, v)) = p50.iter().find(|x| x.0 == "vec") {
        for &(name, t) in p50.iter().filter(|x| x.0 != "vec") {
            let chapter = match (name, shuffle) {
                (_, false) => "only a few times, without the shuffle",
                ("list", true) => "100-300x",
                _ => "10-40x",
            };
            println!("{name} / vec: {:.0}x at the p50 (the chapter: {chapter})", t / v);
        }
    }
    println!("The list's time per node is the most honest memory-latency figure user code gets: compare it \
              with the ladder in section 1.2 and the Lab 1.1 plateau.");
    println!("Write down time per element for both, with and without huge pages.");
}

/// One warm-up traversal, then `reps` timed ones: their summary in ns, and the sum.
fn time(reps: u64, tpn: f64, mut f: impl FnMut() -> u64) -> (Summary, u64) {
    let sum = black_box(f());
    let mut ns = Vec::with_capacity(reps as usize);
    for _ in 0..reps {
        let t0 = now();
        let s = f();
        let t1 = now();
        black_box(s);
        ns.push((t1 - t0) as f64 / tpn);
    }
    (Summary::of(&mut ns), sum)
}

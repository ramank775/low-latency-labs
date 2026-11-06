//! stride: Lab 1.1, the stride walk.
//!
//!     stride [--size MB] [--reps N] [--core N|none] [--huge] [--no-flush]
//!
//! Allocates 256 MB of u64 and writes every page, then for strides of 1, 2,
//! 4, … 4,096 elements sums every stride-th element across the array and
//! prints the time per pass and per access: p50, p99 and max of the passes.

use ch01_hardware::{accesses, calibrate, commas, flush_touched, machine_line, now, pin_line, predict,
                    anon_huge_kb, stride_sum, Buffer, Cli, CoreArg, Summary, STRIDES};
use std::hint::black_box;

fn help() -> String {
    "stride: Lab 1.1, the stride walk.

usage: stride [--size MB] [--reps N] [--core N|none] [--huge] [--no-flush]

  --size MB     the array, in MB of u64 (default 256: 33,554,432 elements)
  --reps N      timed passes per stride, after one warm-up pass (default 11)
  --core N      pin to core N (default 3, or the single CPU `taskset -c` started it on);
                `none` runs unpinned. A refused pin is reported and the run goes on.
  --huge        ask for 2 MB pages (madvise; no root while THP is `madvise` or `always`)
  --no-flush    leave the lines in cache between passes: large strides then fit in
                L2 or L3 and come out fast, the opposite of the chapter's curve

Strides are 1, 2, 4, ... 4,096 elements (8 bytes to 32 KB). Before each pass,
untimed, the lines the pass will touch are flushed (clflush), so every pass
starts from DRAM as the chapter's reasoning assumes.

Run as the chapter does:   taskset -c 3 ./target/release/stride
Huge pages on, to see the TLB misses go:   ./target/release/stride --huge
(check /sys/kernel/mm/transparent_hugepage/enabled; the run prints AnonHugePages)".to_string()
}

fn main() {
    let mut cli = Cli::new("stride", help());
    let (mut size_mb, mut reps, mut core, mut huge, mut flush) = (256u64, 11u64, CoreArg::Default(3), false, true);
    while let Some(f) = cli.flag() {
        match f.as_str() {
            "--size" => size_mb = cli.count("--size"),
            "--reps" => reps = cli.count("--reps"),
            "--core" => core = cli.core("--core"),
            "--huge" => huge = true,
            "--no-flush" => flush = false,
            other => cli.fail(&format!("unknown flag {other}")),
        }
    }
    let len = (size_mb as usize) << 17; // MB of u64: 2^20 / 8 elements each

    let scale = if size_mb == 256 { String::new() } else {
        let b = (size_mb << 20) as f64;
        format!(" (scaled to --size {size_mb}: {:.2}-{:.2} ms per pass)", b / 20e9 * 1e3, b / 15e9 * 1e3)
    };
    predict(&format!(
        "strides of 1 to 8 elements fetch every line once, in order: time per pass flat at about \
         13-17 ms for 256 MB (256 MB / 15-20 GB/s){scale}, time per access rising from about 0.4 ns \
         to 3 ns. On Intel, stride 16 may stay flat too. From 128 B to 2 KB, 5-10 ns per access. \
         From 4 KB on, a new page every access: a knee, then a plateau around 10-30 ns, well below \
         the 90 ns DRAM latency, because the addresses are computed and ten or more misses are in flight."));

    let pinned = pin_line(core); // before the array: its pages are placed by this thread's first touch
    let tpn = calibrate();
    println!("{}", machine_line(tpn));
    println!("{pinned}");

    let mut v = Buffer::new(len, 0u64, huge);
    for (i, x) in v.iter_mut().enumerate() { *x = i as u64; }
    println!("array: {size_mb} MB of u64, {} elements, on a 2 MB boundary, every page written before timing; \
              {} pages asked for, AnonHugePages now {} kB",
             commas(len as u64), if huge { "2 MB" } else { "4 KB" },
             anon_huge_kb().map_or("unknown".to_string(), commas));
    println!("{reps} timed passes per stride after one warm-up; {}",
             if flush { "each pass starts cold: the lines it touches are flushed first (untimed)" }
             else { "--no-flush: lines stay cached between passes" });
    println!();
    println!("   stride    stride      accesses    time per pass (ms)            time per access (ns)");
    println!(" elements     bytes      per pass      p50      p99      max        p50      p99      max     n");

    let mut pass_ns = Vec::with_capacity(reps as usize); // room made before any timing
    for &stride in STRIDES.iter() {
        let n = accesses(len, stride);
        if flush { flush_touched(&v, stride); }
        black_box(stride_sum(black_box(&v), stride)); // warm-up: TLB, predictor, code
        pass_ns.clear();
        for _ in 0..reps {
            if flush { flush_touched(&v, stride); }
            let t0 = now();
            let s = stride_sum(black_box(&v), stride);
            let t1 = now();
            black_box(s);
            pass_ns.push((t1 - t0) as f64 / tpn);
        }
        let p = Summary::of(&mut pass_ns);
        let per = |t: f64| t / n as f64;
        println!(" {stride:>8} {:>9} {:>13} {:>8.3} {:>8.3} {:>8.3}   {:>8.2} {:>8.2} {:>8.2} {:>5}",
                 commas(stride as u64 * 8), commas(n as u64), p.p50 / 1e6, p.p99 / 1e6, p.max / 1e6,
                 per(p.p50), per(p.p99), per(p.max), p.n);
    }
    println!();
    println!("Write down where time per pass stops being flat, the knee, and the plateau.");
    println!("Pitfalls: a slow first stride means page faults were timed (here every page is written first); \
              a surprisingly low plateau may mean huge pages (see AnonHugePages above).");
}

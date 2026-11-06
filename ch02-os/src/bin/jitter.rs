//! jitter: Lab 2.1. Reads `rdtsc` back to back for ten seconds and records
//! every gap into a fixed-range hdrhistogram, allocating nothing in the loop;
//! prints p50, p99, p99.9, p99.99 and max in nanoseconds, the gaps above
//! 1 µs, 10 µs and 100 µs, and the /proc/interrupts column for the core
//! before and after.
//!
//!     jitter [--secs S] [--core N]
//!
//! The chapter's three runs:
//!
//!     ./target/release/jitter                    # unpinned
//!     taskset -c 3 ./target/release/jitter       # pinned to an ordinary core
//!     taskset -c 5 ./target/release/jitter       # pinned to an isolated nohz_full core, if the box has one

use ch02_os::counting::allocs;
use ch02_os::procfs::{sep, Snapshot};
use ch02_os::{above, calibrate, current_cpu, jitter, new_hist, now_unfenced, parse_num, place, print_platform, summary, ToNs};
use std::process::exit;
use std::time::Instant;

const USAGE: &str = "\
usage: jitter [--secs S] [--core N]

Reads rdtsc back to back for S seconds (default 10) and records every gap.
  --secs S   how long to run, in seconds (default 10; the chapter's run is 10)
  --core N   pin to core N with sched_setaffinity; without it the run is
             unpinned, unless started under `taskset -c N`, which the report
             names as pinned

The chapter's three runs:
  ./target/release/jitter                     unpinned
  taskset -c 3 ./target/release/jitter        pinned to an ordinary core
  taskset -c 5 ./target/release/jitter        pinned to an isolated nohz_full core

Prints the /proc/interrupts and /proc/softirqs deltas for the core itself, so
no snapshot by hand is needed. An isolated core needs boot parameters (root,
a reboot): isolcpus=domain,managed_irq,3-7 nohz_full=3-7 rcu_nocbs=3-7.";

fn usage(msg: &str) -> ! {
    eprintln!("jitter: {msg}\n{USAGE}");
    exit(2)
}

fn main() {
    ch02_os::default_sigpipe();
    let (mut secs, mut core) = (10.0f64, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--secs" => secs = val().parse().ok().filter(|s: &f64| *s > 0.0).unwrap_or_else(|| usage("--secs: a number of seconds above 0")),
            "--core" => core = Some(parse_num(&val()).unwrap_or_else(|| usage("--core: a core number")) as usize),
            "-h" | "--help" => { println!("{USAGE}"); return; }
            other => usage(&format!("unknown flag {other}")),
        }
    }

    println!("jitter: rdtsc read back to back for {secs} s, every gap recorded (Lab 2.1)");
    let p = place(core);
    let mode = if p.pinned.is_none() { "unpinned" } else if p.isolated() { "isolated" } else { "pinned" };
    println!("Predict first (the chapter's prediction, before you measure): two rdtsc back to back are \
              25-40 reference ticks apart, 8-12 ns, so p50 and p99 sit there in all three runs and do not move.");
    println!("Predict first, {mode} (the chapter's prediction): {}", match mode {
        "unpinned" => "the tick, device interrupts, the odd slice given to another process and migrations: \
                       hundreds to thousands of gaps over 1 µs per second, p99.99 possibly in the microseconds, \
                       max hundreds of microseconds to milliseconds",
        "pinned" => "migrations go; the tick stays: 250-1,000 gaps a second of 1-5 µs, matching the LOC delta, \
                     and a max of tens of microseconds when a kworker runs",
        _ => "gaps over 1 µs fall to single digits per second (the residual interrupts and little else) and the max \
              to a few microseconds; anything larger that no /proc/interrupts row accounts for is usually an SMI",
    });
    print_platform();
    println!("core: {}", p.note);

    let tpn = calibrate();
    let ns = ToNs::new(tpn);
    println!("tsc: {tpn:.3} ticks/ns");

    // warm up: the loop's code, the histogram's counters, the clock
    let mut h = new_hist();
    jitter(&mut h, now_unfenced() + (0.2e9 * tpn) as u64, ns);
    h.reset();

    let before = Snapshot::take();
    let a0 = allocs();
    let t0 = Instant::now();
    let backwards = jitter(&mut h, now_unfenced() + (secs * 1e9 * tpn) as u64, ns);
    let elapsed = t0.elapsed().as_secs_f64();
    let a1 = allocs();
    let after = Snapshot::take();
    let end_cpu = current_cpu();

    println!();
    println!("gaps: {}", summary(&h));
    let rate = |n: u64| format!("{} ({:.1}/s)", sep(n), n as f64 / elapsed);
    println!("above 1 µs: {}; above 10 µs: {}; above 100 µs: {}",
             rate(above(&h, 1_000)), rate(above(&h, 10_000)), rate(above(&h, 100_000)));
    println!("gaps that came out backwards (a move to a core whose TSC disagrees): {backwards}");
    println!("allocations in the loop: {}", a1 - a0);
    println!("ran for {elapsed:.2} s; on core {end_cpu} at the end");
    println!("deltas over the run:");
    before.report(&after, &p.cores(), elapsed);
    println!("Write down: p50, p99.99, max, the count above 1 µs and the LOC delta for each of the three runs.");
}

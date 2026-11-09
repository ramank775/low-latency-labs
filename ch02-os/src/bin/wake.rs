//! wake: Lab 2.4 (stretch). Two threads pinned to two different physical
//! cores pass a token back and forth a million times: `--spin` through an
//! `AtomicBool` with `spin_loop`, `--park` through `thread::park` and
//! `unpark` (a futex each way), `--sleep` by polling the atomic with
//! `thread::sleep(1 µs)` between polls. `--c0` holds `/dev/cpu_dma_latency`
//! at zero for the run. Reports round-trip p50, p99 and max.
//!
//!     wake [--spin] [--park] [--sleep] [--c0] [--timerslack NS] [--cores A,B] [--iters N] [--warmup N]

use ch02_os::mem;
use ch02_os::procfs::{read_trim, sep, Snapshot};
use ch02_os::{allowed_cores, calibrate, clock_pair_p50, cpu_list, parse_cpu_list, parse_num, ping_pong,
              print_platform, summary, ToNs, Wait};
use std::process::exit;
use std::time::Instant;

const USAGE: &str = "\
usage: wake [--spin] [--park] [--sleep] [--c0] [--timerslack NS] [--cores A,B] [--iters N] [--warmup N]

Two threads on cores A and B (default 3,4: the feed handler's and the order
gateway's in \"The quiet core\") pass a token --iters times (default 1,000,000)
after --warmup round trips (default 10,000). With none of the three variant
flags, all three run, spin first, so each is shown as a ratio to the spin.
  --spin           an AtomicBool and spin_loop
  --park           thread::park and unpark: a futex each way
  --sleep          poll the AtomicBool with thread::sleep(1 µs) between polls
                   (at the chapter's numbers a million round trips take ~2 minutes)
  --c0             hold /dev/cpu_dma_latency at 0 for the run (root, or write
                   permission on the device): no core idles deeper than polling
  --timerslack NS  prctl(PR_SET_TIMERSLACK, NS) on both threads; the chapter's
                   experiment is --sleep --timerslack 1
  --cores A,B      two different physical cores, not hyperthread siblings
  --iters N, --warmup N

Run each variant with and without --c0:
  ./target/release/wake --cores 3,4
  sudo ./target/release/wake --cores 3,4 --c0
  ./target/release/wake --sleep --timerslack 1 --cores 3,4
A core that is not available here is reported and its thread runs unpinned,
labelled. A VM offers no control over C-states at all.";

fn usage(msg: &str) -> ! {
    eprintln!("wake: {msg}\n{USAGE}");
    exit(2)
}

fn main() {
    ch02_os::default_sigpipe();
    let (mut waits, mut c0, mut slack) = (Vec::new(), false, None);
    let (mut cores, mut iters, mut warmup) = ([3usize, 4], 1_000_000u64, 10_000u64);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--spin" => waits.push(Wait::Spin),
            "--park" => waits.push(Wait::Park),
            "--sleep" => waits.push(Wait::Sleep),
            "--c0" => c0 = true,
            "--timerslack" => slack = Some(parse_num(&val()).unwrap_or_else(|| usage("--timerslack: nanoseconds"))),
            "--cores" => {
                let v = parse_cpu_list(&val());
                if v.len() != 2 || v[0] == v[1] { usage("--cores: two different cores, as 3,4"); }
                cores = [v[0], v[1]];
            }
            "--iters" => iters = parse_num(&val()).filter(|&n| n > 0).unwrap_or_else(|| usage("--iters: a number above 0")),
            "--warmup" => warmup = parse_num(&val()).unwrap_or_else(|| usage("--warmup: a number")),
            "-h" | "--help" => { println!("{USAGE}"); return; }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if waits.is_empty() { waits = vec![Wait::Spin, Wait::Park, Wait::Sleep]; }
    waits.dedup();

    let names: Vec<&str> = waits.iter().map(|w| w.name()).collect();
    println!("wake: two threads pass a token {} times: {}{}{} (Lab 2.4)", sep(iters), names.join(", "),
             if c0 { ", --c0" } else { "" }, slack.map_or(String::new(), |s| format!(", --timerslack {s}")));
    println!("Predict first (the chapter's prediction, before you measure): spin moves one cache line each way, \
              25-40 ns a hop on a client part's ring and 50-80 ns on a server's mesh, a round trip of 50-80 ns or \
              100-160 ns. Park pays a futex, the reschedule interrupt, the far core's \
              C-state exit and a scheduler pass: 3-10 µs a hop with C-states capped, 6-20 µs a round trip; without --c0 \
              the p99 shows 50-150 µs hops out of C6. Sleep is bounded below by timer slack, 50-60 µs a hop, so a round \
              trip above 100 µs whatever --c0 does; with --timerslack 1 the hop drops to a few microseconds and stops there.");
    print_platform();

    let allowed = allowed_cores();
    let mut want = [Some(cores[0]), Some(cores[1])];
    for (i, w) in want.iter_mut().enumerate() {
        if !allowed.contains(&cores[i]) {
            println!("warning: core {} is not available here (this process may use {}): the {} thread runs UNPINNED; \
                      pass --cores with two of {}", cores[i], cpu_list(&allowed), ["ping", "pong"][i], cpu_list(&allowed));
            *w = None;
        }
    }
    let siblings = read_trim(&format!("/sys/devices/system/cpu/cpu{}/topology/thread_siblings_list", cores[0])).unwrap_or_default();
    if parse_cpu_list(&siblings).contains(&cores[1]) {
        println!("warning: cores {} and {} are hyperthread siblings ({siblings}): they share an L1, so the spin \
                  looks faster than any real cross-core hop", cores[0], cores[1]);
    }
    let idle = mem::cpuidle_states(cores[1]);
    if idle.is_empty() {
        println!("C-states: core {} exposes no cpuidle states (in a VM the host decides; --c0 can change nothing here)", cores[1]);
    } else {
        let s: Vec<String> = idle.iter().map(|(n, l)| format!("{n} {l} µs")).collect();
        println!("C-states of core {} (exit latency): {}", cores[1], s.join(", "));
    }
    let _hold = if c0 {
        match mem::hold_c0() {
            Ok(f) => { println!("--c0: holding /dev/cpu_dma_latency at 0 for the run"); Some(f) }
            Err(e) => { println!("warning: {e}: these numbers are NOT --c0"); None }
        }
    } else {
        None
    };
    println!("timer slack: {} ns by default for this thread{}", sep(mem::timer_slack().max(0) as u64),
             slack.map_or(String::new(), |s| format!("; set to {s} ns on both threads")));

    let tpn = calibrate();
    let ns = ToNs::new(tpn);
    println!("clock: now() pair p50 {} ns, inside every round trip", clock_pair_p50(100_000, ns));
    let report_cores: Vec<usize> = want.iter().flatten().copied().collect();
    let report_cores = if report_cores.is_empty() { allowed.clone() } else { report_cores };

    let before = Snapshot::take();
    let started = Instant::now();
    let mut spin_p = None;
    println!();
    for &w in &waits {
        if w == Wait::Sleep && slack.is_none() && iters >= 100_000 {
            println!("(sleep: {} round trips at the chapter's 100+ µs each: about {} s)", sep(iters), iters / 10_000);
        }
        let r = ping_pong(w, iters, warmup, want, slack, ns);
        for n in &r.notes { println!("warning: {n}"); }
        let at = |c: Option<usize>| c.map_or("unpinned".to_string(), |c| format!("on core {c}"));
        let (p50, p99) = (r.h.value_at_quantile(0.5), r.h.value_at_quantile(0.99));
        println!("{:<5} round trip: {}", w.name(), summary(&r.h));
        print!("      hop p50 {} ns; ping {}, pong {}; timer slack {} / {} ns; allocations {}",
               sep(p50 / 2), at(r.ping_core), at(r.pong_core), sep(r.slack_ns[0].max(0) as u64), sep(r.slack_ns[1].max(0) as u64), r.allocs);
        match spin_p {
            Some((s50, s99)) if w != Wait::Spin => println!("; vs spin: p50 {:.0}x, p99 {:.0}x",
                                                            p50 as f64 / s50 as f64, p99 as f64 / s99 as f64),
            _ => println!(),
        }
        println!("      ping thread: {}", r.ping_counters[0].line(&r.ping_counters[1]));
        println!("      pong thread: {}", r.pong_counters[0].line(&r.pong_counters[1]));
        if w == Wait::Spin { spin_p = Some((p50.max(1), p99.max(1))); }
    }
    let after = Snapshot::take();
    println!("deltas over the run:");
    before.report(&after, &report_cores, started.elapsed().as_secs_f64());
    println!("Write down: the three round-trip p50s and p99s with and without --c0, and the ratio of each to the spin.");
}

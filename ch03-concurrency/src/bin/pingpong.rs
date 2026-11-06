//! Lab 3.3, ping-pong: two threads bounce a value through one padded
//! `AtomicU64`, one line transfer each way, and thread A times every round trip.
//!
//!     pingpong [--cores A,B] [--iters N] [--warmup N] [--no-pause]
//!     pingpong --placements [--iters N] [--warmup N] [--no-pause]
//!     pingpong --matrix [--iters N] [--warmup N] [--no-pause]

use ch03_concurrency::topo::{available_cores, current_cpu, list, machine, pin_or_say, placement, share, Share};
use ch03_concurrency::{calibrate, clock_pair_p50_ns, new_hist, ns, now, parse_cores, parse_num, predict, report,
                       Padded};
use hdrhistogram::Histogram;
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Release}};
use std::thread;

const HELP: &str = "\
pingpong: Lab 3.3, the round trip of one cache line between two cores.

usage:
  pingpong [--cores A,B] [--iters N] [--warmup N] [--no-pause]
  pingpong --placements [--iters N] [--warmup N] [--no-pause]
  pingpong --matrix [--iters N] [--warmup N] [--no-pause]

Thread A stores an odd value and spins until it sees the next even one; thread B spins
until it sees an odd value and replies with the next even one. After warm-up, A times
each round trip with the clock (rdtsc on x86-64), read on A only, so both ends of every
interval come from one core's counter.

  --cores A,B     the two cores; default 2,3
  --iters N       round trips timed, default 1_000_000 (100_000 per pair with --matrix)
  --warmup N      round trips first, not timed, default 100_000 (10_000 with --matrix)
  --no-pause      spin without spin_loop(): pause takes about 140 cycles on Intel parts
                  since Skylake-SP and can add up to 35 ns a hop. Run with and without.
  --placements    one run for each placement this machine has: hyperthread siblings, two
                  cores sharing an L3, two L3s on one socket, two sockets; then the ratio of
                  the two-socket p50 to the one-L3 p50
  --matrix        every pair of available cores, and the p50 of each

Check the threads are where you think while it runs: ps -eLo tid,psr,comm | grep pingpong";

struct Args {
    cores: Vec<usize>,
    iters: Option<u64>,
    warmup: Option<u64>,
    pause: bool,
    placements: bool,
    matrix: bool,
}

fn usage(msg: &str) -> ! {
    eprintln!("pingpong: {msg}\n\n{HELP}");
    exit(2)
}

fn parse() -> Args {
    let mut a = Args { cores: vec![2, 3], iters: None, warmup: None, pause: true, placements: false, matrix: false };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--cores" => a.cores = parse_cores(&val(), 2, "--cores").unwrap_or_else(|e| usage(&e)),
            "--iters" => a.iters = Some(parse_num(&val(), "--iters").unwrap_or_else(|e| usage(&e))),
            "--warmup" => a.warmup = Some(parse_num(&val(), "--warmup").unwrap_or_else(|e| usage(&e))),
            "--no-pause" => a.pause = false,
            "--placements" => a.placements = true,
            "--matrix" => a.matrix = true,
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if a.iters == Some(0) { usage("--iters must be at least 1"); }
    if a.placements && a.matrix { usage("--placements or --matrix, not both"); }
    a
}

struct Run {
    h: Histogram<u64>,
    a: (bool, Option<usize>), // pinned, the CPU it ran on
    b: (bool, Option<usize>),
}

/// One placement: `warmup` round trips, then `iters` timed ones.
fn round_trips<const PAUSE: bool>(a: usize, b: usize, iters: u64, warmup: u64, tpn: f64) -> Run {
    let ball = Padded(AtomicU64::new(0)); // the one line that moves, on 128 bytes of its own
    let total = warmup + iters;
    thread::scope(|s| {
        let tb = s.spawn(|| {
            let pinned = pin_or_say(Some(b), "B");
            let mut odd = 1;
            for _ in 0..total {
                while ball.load(Acquire) != odd {
                    if PAUSE { std::hint::spin_loop() }
                }
                ball.store(odd + 1, Release); // reply: the next even value
                odd += 2;
            }
            (pinned, current_cpu())
        });
        let ta = s.spawn(|| {
            let pinned = pin_or_say(Some(a), "A");
            let mut h = new_hist();
            for r in 0..total {
                let odd = 2 * r + 1;
                let t0 = now();
                ball.store(odd, Release); // serve: the line goes to B ...
                while ball.load(Acquire) != odd + 1 {
                    if PAUSE { std::hint::spin_loop() }
                } // ... and comes back with B's reply
                let t1 = now();
                if r >= warmup { h.saturating_record(ns(t1 - t0, tpn).max(1)); }
            }
            (h, pinned, current_cpu())
        });
        let (h, ap, ac) = ta.join().expect("thread A");
        Run { h, a: (ap, ac), b: tb.join().expect("thread B") }
    })
}

fn measure(a: usize, b: usize, iters: u64, warmup: u64, pause: bool, tpn: f64) -> Run {
    if pause { round_trips::<true>(a, b, iters, warmup, tpn) } else { round_trips::<false>(a, b, iters, warmup, tpn) }
}

fn side(name: &str, core: usize, (pinned, cpu): (bool, Option<usize>)) -> String {
    let on = cpu.map_or("?".to_string(), |c| c.to_string());
    if pinned { format!("{name} pinned to core {core} (ran on CPU {on})") } else { format!("{name} unpinned (last on CPU {on})") }
}

fn print_run(a: usize, b: usize, r: &Run) {
    println!("  {}, {}", side("A", a, r.a), side("B", b, r.b));
    report("round trip", &r.h);
    let (p50, p99) = (r.h.value_at_quantile(0.50), r.h.value_at_quantile(0.99));
    println!("  one hop, p50 / 2: {} ns; p99 is {:.0}% above p50", p50 / 2,
             (p99 as f64 / p50.max(1) as f64 - 1.0) * 100.0);
}

fn main() {
    let a = parse();
    predict(&[
        "Each round trip moves the line twice, once each way.",
        "Two siblings: tens of ns a round trip, the store buffer and the pipeline, not a transfer.",
        "Two cores sharing an L3: on a client part's ring or one AMD core complex 25-40 ns a hop,",
        "  50-80 ns a round trip; on a server part's mesh 50-80 ns a hop, 100-160 ns a round trip.",
        "Two L3s on one socket: 80-120 ns a hop, 160-240 ns a round trip.",
        "Two sockets: 120-250 ns a hop, 250-500 ns a round trip.",
        "On a quiet pinned box the p99 within 10-20% of the p50, and a max showing the 1-5 us",
        "of a timer tick unless the cores are isolated. The matrix comes out in blocks.",
    ]);
    println!("{}", machine());
    let tpn = calibrate();
    let pause = if a.pause { "spin_loop() (pause) in both spins" } else { "--no-pause: bare spins" };
    println!("clock: {tpn:.3} ticks per ns; clock pair p50 {} ns, inside every sample, not subtracted",
             clock_pair_p50_ns(100_000, tpn));
    println!("spin: {pause}\n");

    if a.matrix {
        matrix(a.iters.unwrap_or(100_000), a.warmup.unwrap_or(10_000), a.pause, tpn);
    } else if a.placements {
        placements(a.iters.unwrap_or(1_000_000), a.warmup.unwrap_or(100_000), a.pause, tpn);
    } else {
        let (x, y) = (a.cores[0], a.cores[1]);
        let (iters, warmup) = (a.iters.unwrap_or(1_000_000), a.warmup.unwrap_or(100_000));
        println!("Lab 3.3, ping-pong, {}", placement(Some(x), Some(y)));
        let r = measure(x, y, iters, warmup, a.pause, tpn);
        print_run(x, y, &r);
    }
}

fn placements(iters: u64, warmup: u64, pause: bool, tpn: f64) {
    let cores = available_cores();
    let classes = [Share::Siblings, Share::OneL3, Share::TwoL3s, Share::TwoSockets];
    let mut p50s = Vec::new();
    for class in classes {
        let pair = cores.iter().enumerate()
            .flat_map(|(i, &x)| cores[i + 1..].iter().map(move |&y| (x, y)))
            .find(|&(x, y)| share(x, y) == class);
        match pair {
            None => println!("Lab 3.3, ping-pong, {}: no such pair among cores {}\n", class.describe(), list(&cores)),
            Some((x, y)) => {
                println!("Lab 3.3, ping-pong, {}", placement(Some(x), Some(y)));
                let r = measure(x, y, iters, warmup, pause, tpn);
                print_run(x, y, &r);
                println!();
                p50s.push((class, r.h.value_at_quantile(0.50)));
            }
        }
    }
    let get = |c: Share| p50s.iter().find(|p| p.0 == c).map(|p| p.1);
    match (get(Share::TwoSockets), get(Share::OneL3)) {
        (Some(two), Some(one)) => println!("two-socket p50 / one-L3 p50: {:.2}", two as f64 / one as f64),
        _ => println!("two-socket p50 / one-L3 p50: not measurable here (needs both placements)"),
    }
}

fn matrix(iters: u64, warmup: u64, pause: bool, tpn: f64) {
    let cores = available_cores();
    let n = cores.len();
    let mut m = vec![vec![0u64; n]; n];
    for i in 0..n {
        for j in i + 1..n {
            let r = measure(cores[i], cores[j], iters, warmup, pause, tpn);
            if !(r.a.0 && r.b.0) { eprintln!("matrix: cores {} and {} not both pinned; cell left blank", cores[i], cores[j]); continue; }
            m[i][j] = r.h.value_at_quantile(0.50);
            m[j][i] = m[i][j];
        }
    }
    println!("Lab 3.3, ping-pong matrix: p50 round trip in ns, {iters} timed round trips a pair");
    print!("{:>5}", "");
    for c in &cores { print!("{c:>6}") }
    println!();
    for (i, row) in m.iter().enumerate() {
        print!("{:>5}", cores[i]);
        for (j, &p50) in row.iter().enumerate() {
            if i == j || p50 == 0 { print!("{:>6}", "-") } else { print!("{p50:>6}") }
        }
        println!();
    }
}

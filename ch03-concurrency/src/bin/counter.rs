//! Lab 3.2, contended counter: four threads, ten million increments each,
//! five ways to keep the count.
//!
//!     counter [--mutex] [--atomic] [--per-thread] [--per-thread-rmw] [--unpadded]
//!             [--threads N] [--iters N] [--cores A,B,C,D] [--batch N]
//!
//! With no variant flag, all five run, cheapest first, and a table compares them.

use ch03_concurrency::topo::{current_cpu, list, machine, pin_or_say, share, thread_ctx_switches, Share};
use ch03_concurrency::{calibrate, commas, new_hist, ns, now, parse_cores, parse_num, predict, report_per,
                       OwnCounter, Padded};
use std::process::exit;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::{Acquire, Relaxed, Release}};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "\
counter: Lab 3.2, a counter four threads bump at once.

usage: counter [--mutex] [--atomic] [--per-thread] [--per-thread-rmw] [--unpadded]
               [--threads N] [--iters N] [--cores A,B,C,D] [--batch N]

  --mutex           one shared Arc<Mutex<u64>>
  --atomic          one shared AtomicU64, fetch_add(1, Relaxed)
  --per-thread      a padded counter per thread, one writer, bumped with a load and a store
  --per-thread-rmw  a padded counter per thread, bumped with fetch_add
  --unpadded        a counter per thread, fetch_add, the four side by side in one struct
  (none)            all five, then a table with each one's ratio to --per-thread

  --threads N       default 4 (at most 8 with --unpadded: one 64-byte line holds eight)
  --iters N         increments per thread, default 10_000_000
  --cores A,B,...   one core per thread; default 2,3,4,5. Four physical cores sharing an L3.
  --batch N         the clock is read once per N increments, default 1_000

Prints the wall time, ns per increment (wall time divided by threads x iters), each
thread's finish time, ns per increment batch by batch (p50, p99, p99.9, max), and each
thread's context switches from getrusage, the count perf stat -e context-switches gives.
The chapter also asks for the futex calls; perf counts them without slowing the threads:
  perf stat -e context-switches,syscalls:sys_enter_futex ./target/release/counter --mutex
(strace -f -c -e trace=futex counts too, but stops each thread at every call, so locks are held longer.)";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mutex,
    Atomic,
    PerThread,
    PerThreadRmw,
    Unpadded,
}

impl Kind {
    fn flag(self) -> &'static str {
        match self {
            Kind::Mutex => "--mutex",
            Kind::Atomic => "--atomic",
            Kind::PerThread => "--per-thread",
            Kind::PerThreadRmw => "--per-thread-rmw",
            Kind::Unpadded => "--unpadded",
        }
    }
}

struct Args {
    kinds: Vec<Kind>,
    threads: usize,
    iters: u64,
    cores: Option<String>,
    batch: u64,
}

fn usage(msg: &str) -> ! {
    eprintln!("counter: {msg}\n\n{HELP}");
    exit(2)
}

fn parse() -> (Args, Vec<usize>) {
    let mut a = Args { kinds: vec![], threads: 4, iters: 10_000_000, cores: None, batch: 1_000 };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--mutex" => a.kinds.push(Kind::Mutex),
            "--atomic" => a.kinds.push(Kind::Atomic),
            "--per-thread" => a.kinds.push(Kind::PerThread),
            "--per-thread-rmw" => a.kinds.push(Kind::PerThreadRmw),
            "--unpadded" => a.kinds.push(Kind::Unpadded),
            "--threads" => a.threads = parse_num(&val(), "--threads").unwrap_or_else(|e| usage(&e)) as usize,
            "--iters" => a.iters = parse_num(&val(), "--iters").unwrap_or_else(|e| usage(&e)),
            "--cores" => a.cores = Some(val()),
            "--batch" => a.batch = parse_num(&val(), "--batch").unwrap_or_else(|e| usage(&e)),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if a.threads == 0 || a.iters == 0 || a.batch == 0 { usage("--threads, --iters and --batch must be at least 1"); }
    if a.kinds.is_empty() {
        a.kinds = vec![Kind::PerThread, Kind::PerThreadRmw, Kind::Unpadded, Kind::Atomic, Kind::Mutex];
    }
    if a.kinds.contains(&Kind::Unpadded) && a.threads > 8 { usage("--unpadded packs at most 8 counters into its line"); }
    let cores = match &a.cores {
        Some(s) => parse_cores(s, a.threads, "--cores").unwrap_or_else(|e| usage(&e)),
        None => (2..2 + a.threads).collect(),
    };
    (a, cores)
}

/// Eight counters side by side in one 64-byte line: separate counters, one line.
#[repr(C, align(64))]
#[derive(Default)]
struct SideBySide([AtomicU64; 8]);

struct Shared {
    mutex: Arc<Mutex<u64>>,
    atomic: Padded<AtomicU64>,
    own: Vec<OwnCounter>,
    own_rmw: Vec<Padded<AtomicU64>>,
    packed: SideBySide,
}

struct ThreadRun {
    stamps: Vec<u64>,
    finish: Instant,
    csw: (u64, u64),
    pinned: bool,
    cpu: Option<usize>,
}

struct Outcome {
    kind: Kind,
    wall: Duration,
    per_inc_ns: f64,
    csw: (u64, u64),
}

/// The measured loop: `bump` `iters` times, the clock read once a batch.
#[inline(always)]
fn hot(iters: u64, batch: u64, stamps: &mut Vec<u64>, mut bump: impl FnMut()) {
    stamps.push(now());
    let mut left = batch;
    for _ in 0..iters {
        bump();
        left -= 1;
        if left == 0 {
            stamps.push(now()); // into capacity reserved before the run
            left = batch;
        }
    }
}

fn run(kind: Kind, a: &Args, cores: &[usize], tpn: f64) -> Outcome {
    let sh = Shared {
        mutex: Arc::new(Mutex::new(0)),
        atomic: Padded(AtomicU64::new(0)),
        own: (0..a.threads).map(|_| OwnCounter::new()).collect(),
        own_rmw: (0..a.threads).map(|_| Padded(AtomicU64::new(0))).collect(),
        packed: SideBySide::default(),
    };
    let ready = AtomicU64::new(0);
    let go = AtomicBool::new(false);
    let (iters, batch) = (a.iters, a.batch);
    let mut t0 = Instant::now();
    let runs: Vec<ThreadRun> = thread::scope(|s| {
        let handles: Vec<_> = (0..a.threads).map(|i| {
            let (sh, ready, go, core) = (&sh, &ready, &go, cores[i]);
            s.spawn(move || {
                let pinned = pin_or_say(Some(core), &format!("thread {i}"));
                let mut stamps = Vec::with_capacity((iters / batch + 2) as usize);
                let mutex = Arc::clone(&sh.mutex); // each thread its own handle to the one lock
                ready.fetch_add(1, Release);
                while !go.load(Acquire) { std::hint::spin_loop() }
                let csw0 = thread_ctx_switches();
                match kind {
                    Kind::Mutex => hot(iters, batch, &mut stamps, || *mutex.lock().expect("no thread panics holding it") += 1),
                    Kind::Atomic => hot(iters, batch, &mut stamps, || { sh.atomic.fetch_add(1, Relaxed); }),
                    Kind::PerThread => { let c = &sh.own[i]; hot(iters, batch, &mut stamps, || c.bump()) }
                    Kind::PerThreadRmw => { let c = &sh.own_rmw[i]; hot(iters, batch, &mut stamps, || { c.fetch_add(1, Relaxed); }) }
                    Kind::Unpadded => { let c = &sh.packed.0[i]; hot(iters, batch, &mut stamps, || { c.fetch_add(1, Relaxed); }) }
                }
                let finish = Instant::now();
                let csw1 = thread_ctx_switches();
                ThreadRun { stamps, finish, csw: (csw1.0 - csw0.0, csw1.1 - csw0.1), pinned, cpu: current_cpu() }
            })
        }).collect();
        while ready.load(Acquire) < a.threads as u64 { std::hint::spin_loop() }
        t0 = Instant::now();
        go.store(true, Release); // every thread starts on this store
        handles.into_iter().map(|h| h.join().expect("counter thread")).collect()
    });

    let want = a.threads as u64 * a.iters;
    let total = match kind {
        Kind::Mutex => *sh.mutex.lock().expect("no thread panicked holding it"),
        Kind::Atomic => sh.atomic.load(Relaxed),
        Kind::PerThread => sh.own.iter().map(OwnCounter::get).sum(),
        Kind::PerThreadRmw => sh.own_rmw.iter().map(|c| c.load(Relaxed)).sum(),
        Kind::Unpadded => sh.packed.0.iter().map(|c| c.load(Relaxed)).sum(),
    };
    let wall = runs.iter().map(|r| r.finish - t0).max().unwrap_or_default();
    let mut h = new_hist();
    for r in &runs {
        for w in r.stamps.windows(2) {
            h.saturating_record(ns(w[1] - w[0], tpn).max(1));
        }
    }
    let csw = runs.iter().fold((0, 0), |acc, r| (acc.0 + r.csw.0, acc.1 + r.csw.1));
    let per_inc_ns = wall.as_nanos() as f64 / want as f64;

    println!("Lab 3.2, {}: {} threads x {} increments", kind.flag(), a.threads, commas(a.iters));
    let placed: Vec<String> = runs.iter().zip(cores).map(|(r, c)| {
        let on = r.cpu.map_or("?".into(), |c| c.to_string());
        if r.pinned { format!("{c} (ran on {on})") } else { format!("unpinned (last on {on})") }
    }).collect();
    println!("  threads on cores: {}", placed.join(", "));
    let verdict = if total == want { "as expected" } else { "WRONG: increments were lost" };
    println!("  total {}, {verdict}", commas(total));
    println!("  wall {:.1} ms, {:.2} ns an increment (wall / {})", wall.as_secs_f64() * 1e3, per_inc_ns, commas(want));
    let fin: Vec<String> = runs.iter().map(|r| format!("{:.1}", (r.finish - t0).as_secs_f64() * 1e3)).collect();
    println!("  finish times, ms after the start: {}", fin.join(", "));
    if h.is_empty() {
        println!("  fewer than {} increments a thread, so no batch was timed", a.batch);
    } else {
        report_per(&format!("ns an increment, batch by batch ({} a batch, all threads)", commas(a.batch)), &h, a.batch);
    }
    println!("  context switches, all threads: {} voluntary, {} involuntary\n", commas(csw.0), commas(csw.1));
    if total != want { exit(1); }
    Outcome { kind, wall, per_inc_ns, csw }
}

fn main() {
    let (a, cores) = parse();
    predict(&[
        "--per-thread: a load and a store on an owned line, about 5 cycles: 1-2 ns an increment,",
        "  10-20 ms for 10 million, the four threads running in parallel.",
        "--per-thread-rmw: a locked instruction each, about 5 ns: 50 ms.",
        "--atomic: 40 million increments on one line: 15-50 ns an increment overall (throughput,",
        "  wall / 40 million; one increment's latency is 50-300 ns), 0.6-2 s, 30-100x --per-thread.",
        "--unpadded: the same range as --atomic: the counters are separate, the line is not.",
        "--mutex: 2-10x --atomic, roughly 1-20 s, hundreds of thousands to millions of futex calls and",
        "  context switches to match; uneven finish times, the first can finish in half the last's time.",
    ]);
    println!("{}", machine());
    let mut pairs: Vec<String> = vec![];
    for i in 0..cores.len() {
        for j in i + 1..cores.len() {
            let s = share(cores[i], cores[j]);
            if s != Share::OneL3 { pairs.push(format!("{} and {}: {}", cores[i], cores[j], s.describe())); }
        }
    }
    println!("placement: cores {}; {}", list(&cores),
             if pairs.is_empty() { "every pair shares one L3".to_string() } else { pairs.join("; ") });
    let tpn = calibrate();
    println!("clock: {tpn:.3} ticks per ns\n");

    let results: Vec<Outcome> = a.kinds.iter().map(|&k| run(k, &a, &cores, tpn)).collect();
    if results.len() > 1 {
        let base = results.iter().find(|r| r.kind == Kind::PerThread).map(|r| r.per_inc_ns);
        println!("{:<17} {:>10} {:>14} {:>13} {:>12} {:>14}", "variant", "wall ms", "ns/increment", "x per-thread",
                 "voluntary cs", "involuntary cs");
        for r in &results {
            let ratio = base.map_or("-".to_string(), |b| format!("{:.1}", r.per_inc_ns / b));
            println!("{:<17} {:>10.1} {:>14.2} {:>13} {:>12} {:>14}", r.kind.flag(), r.wall.as_secs_f64() * 1e3,
                     r.per_inc_ns, ratio, commas(r.csw.0), commas(r.csw.1));
        }
    }
}

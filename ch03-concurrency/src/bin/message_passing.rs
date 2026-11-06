//! Lab 3.1, message passing: two pinned threads, a flag and its data, and the
//! store-buffering test beside it.
//!
//!     message_passing [--relaxed | --release-acquire] [--iters N] [--cores A,B]
//!     message_passing --sb [--release-acquire | --seqcst] [--rounds N] [--warmup N] [--cores A,B]
//!
//! With neither ordering flag, each mode runs both of its variants in turn.

use ch03_concurrency::litmus::{mp_read_acquire, mp_read_relaxed, mp_write_relaxed, mp_write_release,
                               sb_release_acquire, sb_seqcst, Mp, Sb};
use ch03_concurrency::topo::{current_cpu, machine, pin_or_say, placement};
use ch03_concurrency::{calibrate, commas, new_hist, ns, now, parse_cores, parse_num, predict, report, report_per,
                       Padded, SpinBarrier};
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::thread;
use std::time::{Duration, Instant};

const HELP: &str = "\
message_passing: Lab 3.1, the two litmus tests of sections 3.4 and 3.5.

usage:
  message_passing [--relaxed | --release-acquire] [--iters N] [--cores A,B]
  message_passing --sb [--release-acquire | --seqcst] [--rounds N] [--warmup N] [--cores A,B]

Message passing (the default mode): the writer counts k from 1 to --iters, storing
data = k and then flag = k; the reader loads flag and then data, and counts a stale
read whenever data is less than the flag it just saw.
  --relaxed           every access Relaxed
  --release-acquire   the store of flag Release and its load Acquire
  --iters N           default 100_000_000

Store buffering (--sb): each round both threads meet at a spin barrier, then A stores
x = 1 and loads y while B stores y = 1 and loads x; a round counts if both loads
returned 0.
  --release-acquire   stores Release, loads Acquire
  --seqcst            all four accesses SeqCst
  --rounds N          default 10_000_000
  --warmup N          rounds run first and left out of every count, default 10_000

  --cores A,B         writer (or A) and reader (or B); default 2,3. Pick two physical
                      cores that share an L3 (lscpu -e), not hyperthread siblings.

The loom model of both patterns:
  RUSTFLAGS=\"--cfg loom\" cargo test --release -p ch03-concurrency --test mp_loom -- --nocapture
The instructions behind each variant (cargo install cargo-show-asm):
  cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::mp_write_relaxed   (and mp_write_release,
  mp_read_relaxed, mp_read_acquire, sb_release_acquire, sb_seqcst)
On Intel, the replays that keep x86's loads looking ordered (needs perf):
  perf stat -e machine_clears.memory_ordering ./target/release/message_passing --relaxed";

const BATCH: u64 = 1024; // the writer reads the clock once per 1,024 iterations

#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    Relaxed,
    ReleaseAcquire,
    SeqCst,
}

impl Order {
    fn name(self) -> &'static str {
        match self {
            Order::Relaxed => "--relaxed",
            Order::ReleaseAcquire => "--release-acquire",
            Order::SeqCst => "--seqcst",
        }
    }
}

struct Args {
    sb: bool,
    orders: Vec<Order>,
    iters: u64,
    rounds: u64,
    warmup: u64,
    cores: Vec<usize>,
}

fn usage(msg: &str) -> ! {
    eprintln!("message_passing: {msg}\n\n{HELP}");
    exit(2)
}

fn parse() -> Args {
    let mut a = Args { sb: false, orders: vec![], iters: 100_000_000, rounds: 10_000_000, warmup: 10_000,
                       cores: vec![2, 3] };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--relaxed" => a.orders.push(Order::Relaxed),
            "--release-acquire" => a.orders.push(Order::ReleaseAcquire),
            "--seqcst" => a.orders.push(Order::SeqCst),
            "--sb" => a.sb = true,
            "--iters" => a.iters = parse_num(&val(), "--iters").unwrap_or_else(|e| usage(&e)),
            "--rounds" => a.rounds = parse_num(&val(), "--rounds").unwrap_or_else(|e| usage(&e)),
            "--warmup" => a.warmup = parse_num(&val(), "--warmup").unwrap_or_else(|e| usage(&e)),
            "--cores" => a.cores = parse_cores(&val(), 2, "--cores").unwrap_or_else(|e| usage(&e)),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if a.iters == 0 || a.rounds == 0 { usage("--iters and --rounds must be at least 1"); }
    if a.sb {
        if a.orders.contains(&Order::Relaxed) { usage("--sb runs with --release-acquire or --seqcst, not --relaxed"); }
        if a.orders.is_empty() { a.orders = vec![Order::ReleaseAcquire, Order::SeqCst]; }
    } else {
        if a.orders.contains(&Order::SeqCst) { usage("--seqcst is a store-buffering variant: add --sb"); }
        if a.orders.is_empty() { a.orders = vec![Order::Relaxed, Order::ReleaseAcquire]; }
    }
    a
}

fn main() {
    let a = parse();
    if a.sb {
        predict(&[
            "On x86-64, store buffering is the reordering the hardware does show.",
            "Release/Acquire: rounds where both loads return 0, between one in a thousand and one in",
            "a hundred thousand, depending on how tightly the barrier lines the two threads up.",
            "SeqCst: none, each round 10-20 ns slower for the two xchg's.",
            "The barrier dominates a round: 200-500 ns a round, 2-5 s for 10 million.",
        ]);
    } else {
        predict(&[
            "On x86-64: 0 stale reads in 100 million iterations, with --relaxed and with --release-acquire,",
            "and cargo asm shows the two variants are the same instructions.",
            "Each iteration moves the two lines between the cores: 20-100 ns an iteration, 2-10 s a run.",
            "On AArch64, --relaxed can show stale reads, from none to thousands per 100 million",
            "depending on the core, and --release-acquire none.",
        ]);
    }
    println!("{}", machine());
    println!("placement: {}", placement(Some(a.cores[0]), Some(a.cores[1])));
    let tpn = calibrate();
    println!("clock: {tpn:.3} ticks per ns\n");
    for &o in &a.orders {
        if a.sb { store_buffering(o, a.rounds, a.warmup, &a.cores, tpn) } else { message_passing(o, a.iters, &a.cores, tpn) }
    }
}

// ---------------------------------------------------------------- message passing

#[inline(always)]
fn write_loop(m: &Mp, iters: u64, stamps: &mut Vec<u64>, write: impl Fn(&Mp, u64)) {
    stamps.push(now());
    for k in 1..=iters {
        write(m, k);
        if k % BATCH == 0 { stamps.push(now()); } // into capacity reserved before the run
    }
}

#[inline(always)]
fn read_loop(m: &Mp, iters: u64, read: impl Fn(&Mp) -> (u64, u64)) -> (u64, u64) {
    let (mut stale, mut reads) = (0u64, 0u64);
    loop {
        let (f, d) = read(m);
        reads += 1;
        if d < f { stale += 1; } // data older than the flag that announced it
        if f == iters { return (stale, reads); }
    }
}

fn message_passing(o: Order, iters: u64, cores: &[usize], tpn: f64) {
    let m = Mp::new();
    let bar = SpinBarrier::new(2);
    let mut stamps: Vec<u64> = Vec::with_capacity((iters / BATCH + 2) as usize);
    let ((wall, w_pin, w_cpu), (stale, reads, r_pin, r_cpu)) = thread::scope(|s| {
        let reader = s.spawn(|| {
            let pinned = pin_or_say(Some(cores[1]), "reader");
            let mut gen = 0;
            bar.wait(&mut gen);
            let (stale, reads) = match o {
                Order::Relaxed => read_loop(&m, iters, mp_read_relaxed),
                _ => read_loop(&m, iters, mp_read_acquire),
            };
            (stale, reads, pinned, current_cpu())
        });
        let writer = s.spawn(|| {
            let pinned = pin_or_say(Some(cores[0]), "writer");
            let mut gen = 0;
            bar.wait(&mut gen);
            let t = Instant::now();
            match o {
                Order::Relaxed => write_loop(&m, iters, &mut stamps, mp_write_relaxed),
                _ => write_loop(&m, iters, &mut stamps, mp_write_release),
            }
            (t.elapsed(), pinned, current_cpu())
        });
        (writer.join().expect("writer thread"), reader.join().expect("reader thread"))
    });

    let mut h = new_hist();
    for w in stamps.windows(2) {
        h.saturating_record(ns(w[1] - w[0], tpn).max(1));
    }
    println!("Lab 3.1, message passing, {}: {} iterations", o.name(), commas(iters));
    println!("  writer {}, reader {}", where_(cores[0], w_pin, w_cpu), where_(cores[1], r_pin, r_cpu));
    println!("  stale reads: {} of {} reads, {:.1} per 100 million iterations",
             commas(stale), commas(reads), stale as f64 * 1e8 / iters as f64);
    if h.is_empty() {
        println!("  writer: fewer than {BATCH} iterations, so no batch was timed");
    } else {
        report_per(&format!("writer, ns an iteration, batch by batch ({BATCH} a batch)"), &h, BATCH);
    }
    println!("  wall {}: {:.1} ns an iteration (wall / iterations)\n", secs(wall), wall.as_nanos() as f64 / iters as f64);
}

// ---------------------------------------------------------------- store buffering

fn store_buffering(o: Order, rounds: u64, warmup: u64, cores: &[usize], tpn: f64) {
    let sb = Sb::new();
    let b_saw = Padded(AtomicU64::new(0)); // B's load of x, handed to A across the second barrier
    let bar = SpinBarrier::new(2);
    let total = warmup + rounds;
    let ((outcomes, h, wall, a_pin, a_cpu), (b_pin, b_cpu)) = thread::scope(|s| {
        let b = s.spawn(|| {
            let pinned = pin_or_say(Some(cores[1]), "B");
            let mut gen = 0;
            for _ in 0..total {
                bar.wait(&mut gen);
                let v = match o {
                    Order::SeqCst => sb_seqcst(&sb.y, &sb.x),
                    _ => sb_release_acquire(&sb.y, &sb.x),
                };
                b_saw.store(v, Relaxed); // ordered before A's read by the barrier's Release/Acquire
                bar.wait(&mut gen);
            }
            (pinned, current_cpu())
        });
        let a = s.spawn(|| {
            let pinned = pin_or_say(Some(cores[0]), "A");
            let mut gen = 0;
            let mut h = new_hist();
            let mut outcomes = [0u64; 4];
            let mut last = 0;
            let mut t0 = Instant::now();
            for r in 0..total {
                if r == warmup { t0 = Instant::now(); }
                bar.wait(&mut gen);
                let a_saw = match o {
                    Order::SeqCst => sb_seqcst(&sb.x, &sb.y),
                    _ => sb_release_acquire(&sb.x, &sb.y),
                };
                bar.wait(&mut gen);
                let t = now();
                if r >= warmup {
                    outcomes[(a_saw * 2 + b_saw.load(Relaxed)) as usize] += 1;
                    if r > warmup { h.saturating_record(ns(t - last, tpn).max(1)); } // one round, barrier to barrier
                }
                last = t;
                sb.x.store(0, Relaxed); // reset for the next round; its first barrier orders these
                sb.y.store(0, Relaxed);
            }
            (outcomes, h, t0.elapsed(), pinned, current_cpu())
        });
        (a.join().expect("thread A"), b.join().expect("thread B"))
    });

    let name = if o == Order::SeqCst { "SeqCst" } else { "Release/Acquire" };
    println!("Lab 3.1, store buffering, {name}: {} rounds after {} of warm-up", commas(rounds), commas(warmup));
    println!("  A {}, B {}", where_(cores[0], a_pin, a_cpu), where_(cores[1], b_pin, b_cpu));
    println!("  both loads 0: {} rounds, {:.1} per million", commas(outcomes[0]), outcomes[0] as f64 * 1e6 / rounds as f64);
    println!("  outcomes (A's load of y, B's load of x): (0,0) {}  (0,1) {}  (1,0) {}  (1,1) {}",
             commas(outcomes[0]), commas(outcomes[1]), commas(outcomes[2]), commas(outcomes[3]));
    report("round, barrier to barrier", &h);
    println!("  wall {}: {:.1} ns a round (wall / rounds)\n", secs(wall), wall.as_nanos() as f64 / rounds as f64);
}

fn where_(core: usize, pinned: bool, cpu: Option<usize>) -> String {
    let on = cpu.map_or("?".to_string(), |c| c.to_string());
    if pinned { format!("pinned to core {core} (ran on CPU {on})") } else { format!("unpinned (last on CPU {on})") }
}

fn secs(d: Duration) -> String { format!("{:.3} s", d.as_secs_f64()) }

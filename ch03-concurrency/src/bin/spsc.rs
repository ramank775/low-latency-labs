//! Lab 3.4's benchmark: 100 million items through the 16-slot ring of
//! `src/spsc.rs`, between two cores that share an L3, without loom.
//!
//!     spsc [--items N] [--cores A,B]

use ch03_concurrency::spsc::Ring;
use ch03_concurrency::topo::{current_cpu, machine, pin_or_say, placement};
use ch03_concurrency::{calibrate, commas, new_hist, ns, now, parse_cores, parse_num, predict, report_per};
use std::process::exit;
use std::thread;
use std::time::Instant;

const HELP: &str = "\
spsc: Lab 3.4's benchmark, items through a 16-slot single-producer single-consumer ring.

usage: spsc [--items N] [--cores A,B]

  --items N     pushed 1 to N, and checked to arrive as 1 to N; default 100_000_000
  --cores A,B   producer and consumer; default 2,3. Two physical cores sharing an L3.

The ring has no cached copy of the other side's counter, so every push reads the tail
line and every pop the head line, and with 16 slots it swings between full and empty;
the full and empty counts say how often. head and tail sit on padded lines of their
own, so this measures the protocol and not false sharing.

The ring's proof is the loom test, not this run:
  RUSTFLAGS=\"--cfg loom\" cargo test --release -p ch03-concurrency --test spsc_loom -- --nocapture";

const N: usize = 16;
const BATCH: u64 = 1024; // the consumer reads the clock once per 1,024 items

fn usage(msg: &str) -> ! {
    eprintln!("spsc: {msg}\n\n{HELP}");
    exit(2)
}

fn main() {
    let (mut items, mut cores) = (100_000_000u64, vec![2, 3]);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--items" => items = parse_num(&val(), "--items").unwrap_or_else(|e| usage(&e)),
            "--cores" => cores = parse_cores(&val(), 2, "--cores").unwrap_or_else(|e| usage(&e)),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if items == 0 { usage("--items must be at least 1"); }
    predict(&[
        "16 slots and no cached copies: every push reads the tail line, every pop the head line,",
        "and the ring swings between full and empty. 30-100 ns an item, 3-10 s for 100 million.",
    ]);
    println!("{}", machine());
    println!("placement: {}", placement(Some(cores[0]), Some(cores[1])));
    let tpn = calibrate();
    println!("clock: {tpn:.3} ticks per ns\n");

    let ring = Ring::<N>::new();
    let mut stamps: Vec<u64> = Vec::with_capacity((items / BATCH + 2) as usize);
    let t0 = Instant::now();
    let ((full, p_pin, p_cpu), (empty, wrong, c_pin, c_cpu)) = thread::scope(|s| {
        let producer = s.spawn(|| {
            let pinned = pin_or_say(Some(cores[0]), "producer");
            let mut full = 0u64;
            for v in 1..=items {
                // SAFETY: this thread is the ring's only producer.
                while unsafe { ring.try_push(v) }.is_err() {
                    full += 1;
                    std::hint::spin_loop();
                }
            }
            (full, pinned, current_cpu())
        });
        let consumer = s.spawn(|| {
            let pinned = pin_or_say(Some(cores[1]), "consumer");
            let (mut empty, mut wrong) = (0u64, 0u64);
            stamps.push(now());
            for want in 1..=items {
                let got = loop {
                    // SAFETY: this thread is the ring's only consumer.
                    if let Some(v) = unsafe { ring.try_pop() } { break v; }
                    empty += 1;
                    std::hint::spin_loop();
                };
                if got != want { wrong += 1; } // lost or duplicated
                if want % BATCH == 0 { stamps.push(now()); } // into capacity reserved before the run
            }
            (empty, wrong, pinned, current_cpu())
        });
        (producer.join().expect("producer thread"), consumer.join().expect("consumer thread"))
    });
    let wall = t0.elapsed();

    let mut h = new_hist();
    for w in stamps.windows(2) {
        h.saturating_record(ns(w[1] - w[0], tpn).max(1));
    }
    let on = |pinned: bool, core: usize, cpu: Option<usize>| {
        let c = cpu.map_or("?".to_string(), |c| c.to_string());
        if pinned { format!("pinned to core {core} (ran on CPU {c})") } else { format!("unpinned (last on CPU {c})") }
    };
    println!("Lab 3.4, Ring<{N}>: {} items", commas(items));
    println!("  producer {}, consumer {}", on(p_pin, cores[0], p_cpu), on(c_pin, cores[1], c_cpu));
    println!("  arrived as 1 to {}: {}", commas(items),
             if wrong == 0 { "yes, none lost or duplicated".to_string() } else { format!("NO, {} out of place", commas(wrong)) });
    println!("  push found the ring full {} times; pop found it empty {} times", commas(full), commas(empty));
    if h.is_empty() {
        println!("  fewer than {BATCH} items, so no batch was timed");
    } else {
        report_per(&format!("consumer, ns an item, batch by batch ({BATCH} a batch)"), &h, BATCH);
    }
    println!("  wall {:.3} s: {:.1} ns an item (wall / items)", wall.as_secs_f64(), wall.as_nanos() as f64 / items as f64);
    if wrong != 0 { exit(1); }
}

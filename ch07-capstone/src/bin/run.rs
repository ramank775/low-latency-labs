//! run: assembles the three threads, replays the file through them and reports.
//!
//!     run [--paced | --flat] [--book array|tree|vec] [--queue spsc|mutex|mpsc]
//!         [--stall-every MS] [--records N] [--warmup N] [--file PATH]
//!         [--orders PATH] [--decisions PATH]
//!         [--feed-core N] [--strategy-core N] [--gateway-core N]
//!
//! Defaults: paced, array, spsc, no stall, every record of `replay.bin`,
//! 100,000 messages of warm-up, feed on core 2, strategy on core 3, gateway on core 6.
//! A pin that cannot be had, on a core that does not exist here or that a
//! cpuset withholds, stops the run; `none` asks for an unpinned thread outright.

use ch05_structures::counting::Counting;
use ch07_capstone::pipeline::{self, BookKind, Config, Summary};
use ch07_capstone::queue::Queue;
use ch07_capstone::{available_cores, calibrate, clock_pair_p50, lock_memory, Replay, Stage};
use hdrhistogram::Histogram;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::process::exit;

// Counts every allocation in the process, so "allocates nothing after warm-up" is a number.
#[global_allocator]
static GLOBAL: Counting = Counting;

struct Args {
    cfg: Config,
    file: PathBuf,
    orders: Option<PathBuf>,
    decisions: Option<PathBuf>,
}

const USAGE: &str = "usage: run [--paced|--flat] [--book array|tree|vec] [--queue spsc|mutex|mpsc] \
                     [--stall-every MS] [--records N] [--warmup N] [--file PATH] [--orders PATH] \
                     [--decisions PATH] [--feed-core N|none] [--strategy-core N|none] [--gateway-core N|none]";

fn usage(msg: &str) -> ! {
    eprintln!("run: {msg}\n{USAGE}");
    exit(2)
}

// --help is a request, not a mistake: the usage goes to stdout and the exit is 0.
fn help() -> ! {
    println!("run: replays a file through the capstone and reports\n{USAGE}\n\n\
              defaults: --paced, --book array, --queue spsc, --warmup 100000, --file replay.bin,\n\
              feed core 2, strategy core 3, gateway core 6; a pin that cannot be had stops the run.\n\
              The flags and the lab's commands are in ch07-capstone/README.md.");
    exit(0)
}

fn parse() -> Args {
    let mut a = Args {
        cfg: Config { paced: true, book: BookKind::Array, queue: Queue::Spsc, records: usize::MAX,
                      warmup: 100_000, stall_every_ms: 0, feed_core: Some(2), strategy_core: Some(3),
                      gateway_core: Some(6), tpn: 0.0 },
        file: PathBuf::from("replay.bin"),
        orders: None,
        decisions: None,
    };
    let num = |s: String, what: &str| -> u64 {
        s.replace('_', "").parse().unwrap_or_else(|_| usage(&format!("{what}: not a number: {s}")))
    };
    let core = |s: String, what: &str| -> Option<usize> {
        if s == "none" { None } else { Some(s.parse().unwrap_or_else(|_| usage(&format!("{what}: a core number or none")))) }
    };
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--paced" => a.cfg.paced = true,
            "--flat" => a.cfg.paced = false,
            "--book" => a.cfg.book = match val().as_str() {
                "array" => BookKind::Array, "tree" => BookKind::Tree, "vec" => BookKind::Vec,
                b => usage(&format!("--book {b}: array, tree or vec")),
            },
            "--queue" => a.cfg.queue = match val().as_str() {
                "spsc" => Queue::Spsc, "mutex" => Queue::Mutex, "mpsc" => Queue::Mpsc,
                q => usage(&format!("--queue {q}: spsc, mutex or mpsc")),
            },
            "--stall-every" => a.cfg.stall_every_ms = num(val(), "--stall-every"),
            "--records" => a.cfg.records = num(val(), "--records") as usize,
            "--warmup" => a.cfg.warmup = num(val(), "--warmup"),
            "--file" => a.file = PathBuf::from(val()),
            "--orders" => a.orders = Some(PathBuf::from(val())),
            "--decisions" => a.decisions = Some(PathBuf::from(val())),
            "--feed-core" => a.cfg.feed_core = core(val(), "--feed-core"),
            "--strategy-core" => a.cfg.strategy_core = core(val(), "--strategy-core"),
            "--gateway-core" => a.cfg.gateway_core = core(val(), "--gateway-core"),
            "-h" | "--help" => help(),
            other => usage(&format!("unknown flag {other}")),
        }
    }
    a
}

fn main() {
    // a panic on any thread, a refused pin's assert! among them, stops the run:
    // otherwise the other threads spin on, waiting for the one that died
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |p| { report(p); exit(101) }));
    let mut a = parse();

    // before anything else is mapped: every later mapping is filled in and stays in RAM
    let locked = lock_memory();
    if !locked {
        eprintln!("warning: memory not locked (mlockall needs root or `ulimit -l unlimited`): pages may fault during the run");
    }
    let tpn = calibrate();
    a.cfg.tpn = tpn;
    let pair = clock_pair_p50(1_000_000);
    let replay = Replay::open(&a.file).unwrap_or_else(|e| usage(&format!("{}: {e}", a.file.display())));
    if replay.is_empty() { usage("the replay file is empty"); }

    // a core the container or cpuset does not give us is a refused pin: stop, never fall back
    let cores = available_cores();
    for (flag, c) in [("--feed-core", a.cfg.feed_core), ("--strategy-core", a.cfg.strategy_core),
                      ("--gateway-core", a.cfg.gateway_core)] {
        if let Some(id) = c {
            if !cores.contains(&id) {
                usage(&format!("core {id} is not available to this process (allowed: {cores:?}); \
                                give {flag} one that is, or {flag} none to run that thread unpinned"));
            }
        }
    }

    let s = pipeline::replay(&replay, &a.cfg);

    print_report(&a, &s, tpn, pair, locked, &replay);

    if let Some(p) = &a.decisions {
        let f = File::create(p).unwrap_or_else(|e| usage(&format!("{}: {e}", p.display())));
        s.report.log.write_decisions(BufWriter::new(f)).expect("writing the decisions file");
    }
    if let Some(p) = &a.orders {
        let f = File::create(p).unwrap_or_else(|e| usage(&format!("{}: {e}", p.display())));
        let mut w = BufWriter::new(f);
        let n = s.report.log.write_orders(&mut w).expect("writing the orders file");
        w.flush().expect("writing the orders file");
        println!("orders file {}: {n} lines", p.display());
    }
}

/// 1234567 as "1,234,567".
fn n(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}

fn row(name: &str, h: &Histogram<u64>, max_seq: Option<u64>) {
    if h.is_empty() {
        println!("  {name:<8} {:>9}", 0);
        return;
    }
    println!("  {name:<8} {:>9} {:>9} {:>9} {:>9} {:>11}   {}",
             n(h.len()), n(h.value_at_quantile(0.50)), n(h.value_at_quantile(0.99)),
             n(h.value_at_quantile(0.999)), n(h.max()), max_seq.map_or("-".into(), n));
}

fn stage_row(name: &str, st: &Stage) {
    if cfg!(feature = "stages") { row(name, &st.h, Some(st.max_seq)); }
    else { println!("  {name:<8} {:>9}   (stages build only: cargo build --release --features stages)", "-"); }
}

fn read_trim(path: &str) -> String {
    std::fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_else(|_| "n/a".into())
}

/// FNV-1a over the running binary: "same binary" in the repeatability rule is checkable.
fn binary_hash() -> String {
    std::fs::read("/proc/self/exe").map(|b| {
        let h = b.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &x| (h ^ x as u64).wrapping_mul(0x100_0000_01b3));
        format!("{h:016x}")
    }).unwrap_or_else(|_| "n/a".into())
}

fn print_report(a: &Args, s: &Summary, tpn: f64, pair: u64, locked: bool, replay: &Replay) {
    let cfg = &a.cfg;
    let model = std::fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|c| c.lines().find(|l| l.starts_with("model name")).map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string()))
        .unwrap_or_else(|| "unknown CPU".into());
    let clocksource = read_trim("/sys/devices/system/clocksource/clocksource0/current_clocksource");
    let governor = read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor");
    let isolated = read_trim("/sys/devices/system/cpu/isolated");
    let nohz = read_trim("/sys/devices/system/cpu/nohz_full");
    let kernel = read_trim("/proc/sys/kernel/osrelease");
    let core = |c: Option<usize>| c.map_or("unpinned".to_string(), |c| format!("core {c}"));
    let book = match cfg.book { BookKind::Array => "array", BookKind::Tree => "tree", BookKind::Vec => "vec" };
    let queue = match cfg.queue { Queue::Spsc => "spsc", Queue::Mutex => "mutex", Queue::Mpsc => "mpsc" };

    println!("capstone run");
    println!("  mode {}, book {book}, queue {queue}, stall-every {} ms, build {}",
             if cfg.paced { "paced" } else { "flat" }, cfg.stall_every_ms,
             if cfg!(feature = "stages") { "stages (t0 t1 t2 t3 t4)" } else { "headline (t0 t3 t4)" });
    println!("  file {} ({} records), replayed {}, warm-up {} (seq {}..{} left out), binary fnv64 {}",
             a.file.display(), n(replay.len() as u64), n(s.records as u64), n(cfg.warmup),
             n(s.first_seq), n(s.warm_seq.saturating_sub(1)), binary_hash());
    println!("  machine: {model}; kernel {kernel}; {} CPUs allowed; clocksource {clocksource}; governor {governor}; isolated [{isolated}]; nohz_full [{nohz}]",
             available_cores().len());
    println!("  clock: TSC {:.1} MHz by calibrate(); back-to-back now() p50 {pair} ticks = {:.1} ns, in every interval below",
             tpn * 1000.0, pair as f64 / tpn);
    println!("  cores: feed {}, strategy {}, gateway {}; memory {}; replay {}, {} pages touched",
             core(cfg.feed_core), core(cfg.strategy_core), core(cfg.gateway_core),
             if locked { "locked (mlockall)" } else { "NOT locked" },
             if replay.locked { "mlocked" } else { "not mlocked" }, n(replay.pages as u64));
    println!("  book window base {} ticks, {} levels a side", s.base, ch07_capstone::LEVELS);

    let secs = s.elapsed_ns as f64 / 1e9;
    let batches = s.report.log.v.len() as u64;
    println!();
    println!("throughput: {} updates in {:.3} s, {} a second; {} batches (decisions-file lines), {:.2} messages a batch; {} orders through the gateway",
             n(s.feed.pushed), secs, n((s.feed.pushed as f64 / secs.max(1e-9)) as u64), n(batches),
             s.feed.pushed as f64 / batches.max(1) as f64, n(s.orders));
    println!();
    println!("latency, ns, messages after warm-up:");
    println!("  {:<8} {:>9} {:>9} {:>9} {:>9} {:>11}   max at seq", "", "count", "p50", "p99", "p99.9", "max");
    let h = &s.report.h;
    stage_row("decode", &h.decode);
    stage_row("hop", &h.hop);
    stage_row("think", &h.think);
    let worst = h.worst.sorted();
    row("total", &h.total, worst.first().map(|w| w.1));
    row("out_hop", &s.out_hop, None);

    println!();
    println!("worst {} total (ns@seq), slowest first:", worst.len());
    for chunk in worst.chunks(8) {
        let line: Vec<String> = chunk.iter().map(|(ns, seq)| format!("{}@{}", n(*ns), seq)).collect();
        println!("  {}", line.join("  "));
    }

    println!();
    println!("must read zero:");
    println!("  sequence gaps            {}   (handler state at the end: {:?})",
             u8::from(s.report.state != ch07_capstone::BookState::Live), s.report.state);
    println!("  orders dropped           {}   (output ring full)", s.report.h.dropped);
    println!("  backwards intervals      {}   (a later stamp below an earlier one: two cores' counters disagree)", n(s.backwards));
    println!("  decisions not logged     {}   (log full)", s.report.log.full);
    println!("  undecodable records      {}", s.feed.bad);
    match s.allocs {
        Some((al, fr)) => println!("  allocations after warm-up {}   (frees {}; counting allocator, every thread)", n(al), n(fr)),
        None => println!("  allocations after warm-up n/a (warm-up never ended, or the counting allocator is not installed)"),
    }
    for t in &s.threads {
        let ran = |st: Option<ch07_capstone::stats::ThreadStats>| st.map_or("?".to_string(), |x| x.cpu.to_string());
        match t.after_warmup() {
            Some(d) => println!("  {:<8} {}, on cpu {} at warm-up, {} at the end: page faults {} (major {}), context switches {} (voluntary {}, involuntary {}), migrations {}",
                                t.name, core(t.core), ran(t.warm), t.cpu_end, n(d.minflt + d.majflt), d.majflt,
                                n(d.voluntary + d.involuntary), n(d.voluntary), n(d.involuntary),
                                d.migrations.map_or("n/a".into(), n)),
            None => println!("  {:<8} {}: kernel counters not read (/proc unavailable, or the warm-up never ended)", t.name, core(t.core)),
        }
    }
    println!();
    println!("feed: {} messages released more than 1 us after their tick; waited on a full queue {} times",
             n(s.feed.late), n(s.feed.full));
}

//! parse: Lab 4.4, parse it. 256 depth updates generated from a fixed seed,
//! each written twice into buffers allocated once: the chapter's 48-byte
//! binary layout and the same fields as JSON shaped like its 194-byte example.
//! Then 10 million parses, cycling through the 256 after a million of warm-up,
//! each result handed to `black_box`.
//!
//!     parse --binary | --json | --json-borrowed | --json-value
//!           [--messages N] [--warmup N] [--seed S] [--core N]

use std::hint::black_box;
use std::process::exit;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

use ch04_network::counting::{Counting, ALLOCS, FREES};
use ch04_network::gen::Messages;
use ch04_network::json::{self, DepthMsgOwned};
use ch04_network::sys::{self, pin_or_say};
use ch04_network::timing::{self, calibrate, commas, new_hist, now};
use ch04_network::{read_fields, Fields};

// Counts every allocation in the process, so "allocates nothing" is a number.
#[global_allocator]
static GLOBAL: Counting = Counting;

const SET: usize = 256;

const HELP: &str = "\
parse: Lab 4.4, parse it: 256 seeded depth updates in the chapter's 48-byte binary layout and as its
194-byte JSON, parsed --messages times after --warmup, cycling through the 256, each result through
black_box. Prints nanoseconds per message (p50 / p99 / max over batches of 256, and the chapter's
measured-loop wall time over the count) and allocations per message from a counting allocator.

usage: parse <mode> [--messages N] [--warmup N] [--seed S] [--core N]

modes:
  --binary         a zerocopy overlay through ref_from_prefix, the type check, five fields copied out
  --json           serde_json::from_slice into an owned struct with six String fields, then the price
                   and quantity turned into integer ticks and lots
  --json-borrowed  the same into a struct of &str fields borrowed from the buffer
  --json-value     into a serde_json::Value, the same fields read out of it

options:
  --messages N     parses timed (default 10,000,000)
  --warmup N       parses before timing (default 1,000,000)
  --seed S         the generator's seed (default 7)
  --core N         the core to pin to (default 2, the feed thread's)

There is no perf on the reference VM; where there is, count what each message costs:
  perf stat -e cycles,instructions,branches,branch-misses ./target/release/parse --binary
and divide each count by messages + warm-up, 11 million at the defaults. Where the JSON time goes:
  perf record -g ./target/release/parse --json && perf report
The binary path's instructions: cargo asm or objdump -d on ch04_network::read_fields should show two
compares, two branches and a handful of movs. strace -c should show the same few dozen calls for
every mode, all at startup.";

#[derive(Clone, Copy, PartialEq)]
enum Mode { Binary, Json, JsonBorrowed, JsonValue }

fn usage(msg: &str) -> ! {
    eprintln!("parse: {msg}\n\n{HELP}");
    exit(2)
}

fn prediction(m: Mode) -> &'static [&'static str] {
    match m {
        Mode::Binary => &["--binary: a length check, a type check, five loads and five stores: a dozen or two instructions,",
                          "branches that always go the same way, no allocation. The chapter's 10-20 ns (40-80 cycles at 4 GHz),",
                          "or a little less, since in a loop the core overlaps one message's loads with the next's. 0 allocations."],
        Mode::JsonBorrowed => &["--json-borrowed: about 194 bytes walked, nine keys compared, 29 digits converted, fifteen strings",
                                "scanned, two decimal strings turned into integers: thousands of instructions, several hundred",
                                "branches. Warm, as this loop is: under 1 us (1-3 us in a running handler). 0 allocations."],
        Mode::Json => &["--json: --json-borrowed plus six allocations and six frees at 20-50 ns each, 240-600 ns more:",
                        "around 1 us. 6 allocations a message."],
        Mode::JsonValue => &["--json-value: seventeen allocations and frees (a map for each of the two objects, a String for",
                             "each of the nine keys and six string values), 680-1,700 ns before building the maps: into the",
                             "1-3 us range even this warm. 17 allocations a message."],
    }
}

/// The measured loop, generic so each mode is compiled on its own: batches of
/// the 256 messages, each batch timed with two TSC reads, nothing allocated
/// but what the parse itself allocates.
fn run(frames: &[&[u8]], messages: u64, warmup: u64, tpn: f64, mut one: impl FnMut(&[u8]) -> Option<Fields>)
    -> (hdrhistogram::Histogram<u64>, u64, std::time::Duration, u64, u64, u64)
{
    let mut h = new_hist();
    let mut fails = 0u64;
    for k in 0..warmup as usize {
        if black_box(one(frames[k % frames.len()])).is_none() { fails += 1; }
    }
    let batches = messages.div_ceil(frames.len() as u64);
    let (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
    let wall = Instant::now();
    for _ in 0..batches {
        let t0 = now();
        for &f in frames {
            if black_box(one(f)).is_none() { fails += 1; }
        }
        let t1 = now();
        h.saturating_record(((t1 - t0) as f64 / tpn) as u64);
    }
    let wall = wall.elapsed();
    let (a1, f1) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
    (h, batches * frames.len() as u64, wall, a1 - a0, f1 - f0, fails)
}

fn main() {
    let mut mode = None;
    let (mut messages, mut warmup, mut seed, mut core) = (10_000_000u64, 1_000_000u64, 7u64, Some(2usize));
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        let mut set = |m| { if mode.is_some() { usage("give one mode"); } mode = Some(m); };
        match flag.as_str() {
            "--binary" => set(Mode::Binary),
            "--json" => set(Mode::Json),
            "--json-borrowed" => set(Mode::JsonBorrowed),
            "--json-value" => set(Mode::JsonValue),
            "--messages" => messages = timing::count(&val()).unwrap_or_else(|| usage("--messages: a count")),
            "--warmup" => warmup = timing::count(&val()).unwrap_or_else(|| usage("--warmup: a count")),
            "--seed" => seed = val().parse().unwrap_or_else(|_| usage("--seed: an integer")),
            "--core" => { let v = val(); core = if v == "none" { None } else { Some(v.parse().unwrap_or_else(|_| usage("--core: a core number"))) } }
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    let mode = mode.unwrap_or_else(|| usage("give a mode: --binary, --json, --json-borrowed or --json-value"));
    if messages == 0 { usage("--messages must be at least 1"); }
    if !ch04_network::counting::installed() { usage("the counting allocator is not installed"); }

    timing::predict(prediction(mode));
    println!("  Every JSON mode tens of times --binary or more: --json-borrowed at the low end, --json-value well over");
    println!("  a hundred. (--json - --json-borrowed) / 6 is one allocation");
    println!("  and its free, 40-100 ns, or somewhat less in a loop this warm.");
    let tpn = calibrate();
    let m = Messages::new(SET, seed);
    let (lo, hi) = (0..SET).map(|i| m.json_msg(i).len()).fold((usize::MAX, 0), |(a, b), l| (a.min(l), b.max(l)));
    // where each message lies, recorded once, so the timed loop does no framing
    let frames: Vec<&[u8]> = (0..SET).map(|i| if mode == Mode::Binary { m.bin_msg(i) } else { m.json_msg(i) }).collect();
    pin_or_say(core, "parse");
    let name = match mode { Mode::Binary => "--binary", Mode::Json => "--json", Mode::JsonBorrowed => "--json-borrowed",
                            Mode::JsonValue => "--json-value" };
    println!("parse {name}: {}, {SET} updates (seed {seed}; binary 48 bytes, JSON {lo}-{hi} bytes), {} parses after {} of warm-up",
             sys::describe_box(tpn), commas(messages), commas(warmup));

    let (h, n, wall, allocs, frees, fails) = match mode {
        Mode::Binary => run(&frames, messages, warmup, tpn, |b| {
            let mut f = Fields::default();
            read_fields(b, &mut f).then_some(f)
        }),
        Mode::Json => run(&frames, messages, warmup, tpn, |b| serde_json::from_slice::<DepthMsgOwned>(b).ok()?.fields()),
        Mode::JsonBorrowed => run(&frames, messages, warmup, tpn, |b| json::borrowed(b).ok()?.fields()),
        Mode::JsonValue => run(&frames, messages, warmup, tpn, |b| json::value_fields(&serde_json::from_slice(b).ok()?)),
    };
    if fails > 0 { eprintln!("parse: {fails} messages failed to parse"); exit(1); }

    let per = |v: u64| v as f64 / SET as f64;
    println!("parse {name}: per message, over {} batches of {SET}: p50 {:.1} ns  p99 {:.1} ns  p99.9 {:.1} ns  max {:.1} ns",
             commas(h.len()), per(h.value_at_quantile(0.50)), per(h.value_at_quantile(0.99)),
             per(h.value_at_quantile(0.999)), per(h.max()));
    let wall_per = wall.as_nanos() as f64 / n as f64;
    println!("  the chapter's figure, the measured loop's wall time over {} messages: {wall_per:.1} ns a message, \
              {:.0} TSC ticks at {tpn:.2} GHz, {:.0} cycles at the chapter's 4 GHz", commas(n), wall_per * tpn, wall_per * 4.0);
    println!("  allocations per message: {:.2} (frees {:.2}); allocator: the system's (glibc malloc), through the counting wrapper",
             allocs as f64 / n as f64, frees as f64 / n as f64);
    println!("  perf stat here would divide by {} parses (messages + warm-up)", commas(n + warmup));
}

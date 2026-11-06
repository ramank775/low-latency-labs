//! gen: writes a synthetic replay file of 56-byte records.
//!
//!     gen [--records N] [--rate MSGS_PER_S] [--seed S] [--out PATH]
//!
//! Defaults: 2,000,000 records at 100,000 a second (20 seconds of market,
//! 112 MB), seed 1, written to `replay.bin`. The same seed writes the same bytes.

use ch07_capstone::synth::{self, Params};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::process::exit;

const USAGE: &str = "usage: gen [--records N] [--rate MSGS_PER_S] [--seed S] [--out PATH]";

fn usage(msg: &str) -> ! {
    eprintln!("gen: {msg}\n{USAGE}");
    exit(2)
}

// --help is a request, not a mistake: the usage goes to stdout and the exit is 0.
fn help() -> ! {
    println!("gen: writes a synthetic replay file of 56-byte records\n{USAGE}\n\n\
              defaults: 2,000,000 records at 100,000 a second (20 s of market, 112 MB), seed 1, into replay.bin;\n\
              the same seed writes the same bytes");
    exit(0)
}

fn main() {
    let mut p = Params::default();
    let mut out = String::from("replay.bin");
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{a} needs a value")));
        match a.as_str() {
            "--records" => p.records = val().replace('_', "").parse().unwrap_or_else(|_| usage("--records: a count")),
            "--rate" => p.rate = val().replace('_', "").parse().unwrap_or_else(|_| usage("--rate: messages a second")),
            "--seed" => p.seed = val().parse().unwrap_or_else(|_| usage("--seed: an integer")),
            "--out" => out = val(),
            "-h" | "--help" => help(),
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if p.records == 0 || p.rate <= 0.0 { usage("--records and --rate must be positive"); }

    let file = File::create(&out).unwrap_or_else(|e| usage(&format!("{out}: {e}")));
    let mut w = BufWriter::with_capacity(1 << 20, file);
    let written = synth::write(&p, &mut w).and_then(|s| w.flush().map(|_| s))
        .unwrap_or_else(|e| usage(&format!("writing {out}: {e}")));

    let pct = |n: u64| 100.0 * n as f64 / written.records as f64;
    let secs = written.last_arrival_ns as f64 / 1e9;
    println!("wrote {out}: {} records, {} bytes, seed {}", written.records, written.records * 56, p.seed);
    println!("  {:.2} s of market at {:.0} messages a second on average; {} bursts of up to 30",
             secs, written.records as f64 / secs.max(1e-9), written.bursts);
    println!("  deep {:.1}%, best quantity {:.1}%, best price moves {:.1}%; the rest open the book or trim its ends",
             pct(written.deep), pct(written.best_qty), pct(written.best_moves));
    println!("  instrument {}, seq 1..={}, prices {}..{} ticks", p.instrument, written.records,
             written.min_price, written.max_price);
}

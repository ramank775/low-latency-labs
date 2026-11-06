//! sum: section 1.6's worked example, one accumulator or four.
//!
//!     sum [--len N] [--iters N] [--core N|none] [--seed S]
//!
//! Times `sum1` and `sum4` over 4,096 f64 values (32 KB, resident in L1),
//! one call per sample.

use ch01_hardware::{calibrate, commas, machine_line, now, pin_line, predict, sum1, sum4, Cli, CoreArg, Rng, Summary};
use std::hint::black_box;

fn help() -> String {
    "sum: section 1.6's worked example, one accumulator or four.

usage: sum [--len N] [--iters N] [--core N|none] [--seed S]

  --len N     values summed per call (default 4,096: 32 KB, resident in L1)
  --iters N   timed calls per function, after 1,000 warm-up calls (default 100,000)
  --core N    pin to core N (default 3, or the single CPU `taskset -c` started it on);
              `none` runs unpinned. A refused pin is reported and the run goes on.
  --seed S    the values' seed (default 7)

The instructions and the data are the same; only the dependency structure differs.
With perf, cycles per add: perf stat -e cycles,instructions ./target/release/sum".to_string()
}

#[inline(never)]
fn one(v: &[f64]) -> f64 { sum1(v) }

#[inline(never)]
fn four(v: &[f64]) -> f64 { sum4(v) }

fn main() {
    let mut cli = Cli::new("sum", help());
    let (mut len, mut iters, mut core, mut seed) = (4096u64, 100_000u64, CoreArg::Default(3), 7u64);
    while let Some(f) = cli.flag() {
        match f.as_str() {
            "--len" => len = cli.count("--len"),
            "--iters" => iters = cli.count("--iters"),
            "--core" => core = cli.core("--core"),
            "--seed" => seed = cli.value("--seed").parse().unwrap_or_else(|_| cli.fail("--seed: an integer")),
            other => cli.fail(&format!("unknown flag {other}")),
        }
    }

    let scale = if len == 4096 { String::new() } else {
        format!(" (at --len {len}: sum1 about {:.2} us, sum4 about {:.2} us)", len as f64 * 4.0 / 4e3, len as f64 / 4e3)
    };
    predict(&format!(
        "a floating-point add takes 3-4 cycles and the core can start two per cycle. sum1 is one \
         chain: for 4,096 values about 16,000 cycles, roughly 4 us at 4 GHz. sum4 is four chains \
         that interleave, about one add per cycle: roughly 1 us, four times faster{scale}."));

    let pinned = pin_line(core);
    let tpn = calibrate();
    println!("{}", machine_line(tpn));
    println!("{pinned}");
    let mut rng = Rng::new(seed);
    let v: Vec<f64> = (0..len).map(|_| (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64).collect();
    println!("{} f64 values in 0..1 (seed {seed}), {} KB; {} timed calls per function after 1,000 warm-up calls",
             commas(len), len * 8 / 1024, commas(iters));
    println!();
    println!(" function   time per call (ns)                          time per add (ns)    result");
    println!("            p50       p99     p99.9       max      n       p50");

    let mut ns = Vec::with_capacity(iters as usize);
    let mut p50 = [0.0; 2];
    for (k, (name, f)) in [("sum1", one as fn(&[f64]) -> f64), ("sum4", four)].into_iter().enumerate() {
        let mut r = 0.0;
        for _ in 0..1000 { r = black_box(f(black_box(&v))); } // warm-up: L1, predictor, clock
        ns.clear();
        for _ in 0..iters {
            let t0 = now();
            let s = f(black_box(&v));
            let t1 = now();
            black_box(s);
            ns.push((t1 - t0) as f64 / tpn);
        }
        let p = Summary::of(&mut ns);
        println!(" {name:<8} {:>9.0} {:>9.0} {:>9.0} {:>9.0} {:>6}   {:>9.3}       {r:.17}",
                 p.p50, p.p99, p.p999, p.max, p.n, p.p50 / len as f64);
        p50[k] = p.p50;
    }
    println!();
    println!("sum1 / sum4: {:.2}x at the p50 (the chapter: about 4x). The two results may differ in the \
              last bits: floating-point addition is not associative, so the compiler will not split the chain for you.",
             p50[0] / p50[1]);
}

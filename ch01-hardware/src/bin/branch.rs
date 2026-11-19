//! branch: Lab 1.2, branches.
//!
//!     branch [--variant unsorted|sorted|branchless|vector|all] [--size MB] [--reps N]
//!            [--core N|none] [--seed S]
//!
//! Fills 100 MB with uniform random bytes and sums every byte greater than
//! 128: branchy over the unsorted data, branchy over a sorted copy (the sort
//! is not timed), and the conditional-move rewrite over the unsorted data;
//! `vector` is the same select with the compiler left free to vectorise it.

use ch01_hardware::{calibrate, commas, counting_sort, machine_line, now, pin_line, predict, random_bytes,
                    sum_branchless, sum_branchy, sum_vector, Cli, CoreArg, Summary};
use std::hint::black_box;

const VARIANTS: [(&str, &str); 4] = [
    ("unsorted", "branchy, unsorted data"),
    ("sorted", "branchy, the sorted copy"),
    ("branchless", "cmov rewrite, unsorted data"),
    ("vector", "select, vectorised by LLVM"),
];

fn help() -> String {
    "branch: Lab 1.2, branches.

usage: branch [--variant NAME] [--size MB] [--reps N] [--core N|none] [--seed S]

  --variant NAME  one of the variants below, or all (default all)
  --size MB       the random bytes, in MB (default 100)
  --reps N        timed passes per variant, after one warm-up pass (default 11)
  --core N        pin to core N (default 3, or the single CPU `taskset -c` started it on);
                  `none` runs unpinned. A refused pin is reported and the run goes on.
  --seed S        the random bytes' seed (default 7)

variants (each sums every byte greater than 128):
  unsorted     branchy: `if b > 128 { sum += b }`, a real jump per byte, over the random bytes
  sorted       the same branchy loop over a sorted copy (counting sort, untimed)
  branchless   the conditional-move rewrite: `sum += if b > 128 { b } else { 0 }`, kept scalar
  vector       the same select with the compiler left free: it vectorises the loop

The branchy loop carries an empty asm! statement in its taken arm, so LLVM cannot
turn it into a cmov or vector code by itself (the lab's pitfall). Check the assembly:
  objdump -d -C -M intel --no-show-raw-insn target/release/branch | grep -A30 'sum_branchy>:'
  (a jump such as `ja`/`jbe` inside the loop is a branch; `cmov` in sum_branchless is a select)

Run each variant on its own under perf (needs the perf tool; inside a VM it may
print <not supported>):
  perf stat -e branches,branch-misses,cycles,instructions ./target/release/branch --variant unsorted
  perf stat -e branches,branch-misses,cycles,instructions ./target/release/branch --variant sorted
  perf stat -e branches,branch-misses,cycles,instructions ./target/release/branch --variant branchless
perf counts the whole run: reps + 1 passes, plus the untimed fill and sort, which add
almost no branch misses. IPC is instructions / cycles.".to_string()
}

fn main() {
    let mut cli = Cli::new("branch", help());
    let (mut variant, mut size_mb, mut reps, mut core, mut seed) =
        ("all".to_string(), 100u64, 11u64, CoreArg::Default(3), 7u64);
    while let Some(f) = cli.flag() {
        match f.as_str() {
            "--variant" => variant = cli.value("--variant"),
            "--size" => size_mb = cli.count("--size"),
            "--reps" => reps = cli.count("--reps"),
            "--core" => core = cli.core("--core"),
            "--seed" => seed = cli.value("--seed").parse().unwrap_or_else(|_| cli.fail("--seed: an integer")),
            other => cli.fail(&format!("unknown flag {other}")),
        }
    }
    let chosen: Vec<&str> = if variant == "all" {
        VARIANTS.iter().map(|v| v.0).collect()
    } else if let Some(v) = VARIANTS.iter().find(|x| x.0 == variant) {
        vec![v.0]
    } else {
        cli.fail(&format!("--variant {variant}: unsorted, sorted, branchless, vector or all"))
    };
    let n = (size_mb as usize) << 20;

    predict(&format!(
        "on random bytes the condition is true for 127 of 256 values, close to a coin flip: about 50 \
         million mispredicts per 100 MB pass at an effective 24 cycles (the 15-20-cycle refill and the \
         work lost around it), about 0.3 s at 4 GHz, plus 45-70 ms of basic work, two to three cycles a \
         byte. Sorted: the predictor fails only at the boundary, 45-70 ms, set by where the loop lands \
         (a jump across a 32-byte boundary keeps it out of the micro-op cache on Skylake-family cores; \
         compare idq.dsb_uops with idq.mite_uops; this repository pads its jumps clear, 56-59 ms here). Branchless never mispredicts, but its select costs \
         more per byte than a predicted branch: about 60 ms, a little less if vectorised. So \
         unsorted-branchy 4.5-9x slower than either, about 50 million branch misses against near zero, \
         IPC about 0.5 against 2.5 or more. Keep this core's hyperthread sibling idle: a busy one \
         doubles the branchy loop's time.{}",
        if size_mb == 100 { String::new() } else { format!(" (at --size {size_mb}: {size_mb}/100 of those times and misses)") }));

    let pinned = pin_line(core);
    let tpn = calibrate();
    println!("{}", machine_line(tpn));
    println!("{pinned}");

    let data = random_bytes(n, seed);
    let sorted = if chosen.contains(&"sorted") { counting_sort(&data) } else { Vec::new() };
    let above = data.iter().filter(|&&b| b > 128).count();
    println!("data: {size_mb} MB of uniform random bytes (seed {seed}), {:.2}% above 128; {reps} timed passes \
              per variant after one warm-up", 100.0 * above as f64 / n as f64);
    println!();
    println!(" variant     what                            time per pass (ms)          time per byte (ns)        sum");
    println!("                                             p50      p99      max       p50     p99     max     n");

    let mut pass_ns = Vec::with_capacity(reps as usize);
    let mut p50 = Vec::new();
    let mut sums = Vec::new();
    for &name in &chosen {
        let what = VARIANTS.iter().find(|v| v.0 == name).map_or("", |v| v.1);
        let run = |d: &[u8]| match name {
            "unsorted" | "sorted" => sum_branchy(d),
            "branchless" => sum_branchless(d),
            _ => sum_vector(d),
        };
        let input: &[u8] = if name == "sorted" { &sorted } else { &data };
        let s = black_box(run(black_box(input))); // warm-up: page cache, predictor, code
        pass_ns.clear();
        for _ in 0..reps {
            let t0 = now();
            let r = run(black_box(input));
            let t1 = now();
            black_box(r);
            pass_ns.push((t1 - t0) as f64 / tpn);
        }
        let p = Summary::of(&mut pass_ns);
        let per = |t: f64| t / n as f64;
        println!(" {name:<11} {what:<30} {:>8.2} {:>8.2} {:>8.2}   {:>7.3} {:>7.3} {:>7.3} {:>5}   {}",
                 p.p50 / 1e6, p.p99 / 1e6, p.max / 1e6, per(p.p50), per(p.p99), per(p.max), p.n, commas(s));
        p50.push((name, p.p50));
        sums.push(s);
    }
    println!();
    if sums.windows(2).any(|w| w[0] != w[1]) {
        println!("ERROR: the variants disagree on the sum");
        std::process::exit(1);
    }
    if let Some(&(_, slow)) = p50.iter().find(|x| x.0 == "unsorted") {
        for &(name, t) in p50.iter().filter(|x| x.0 != "unsorted") {
            println!("unsorted / {name}: {:.1}x at the p50 (the chapter: 4.5-9x against sorted and branchless)", slow / t);
        }
    }
    println!("Under perf stat, one variant per run: expect about {} branch-misses per pass for unsorted, \
              near zero for the others; this run makes {} passes per variant.",
             commas(n as u64 / 2), reps + 1); // a coin flip: half the bytes
    println!("Write down time, branch misses and IPC for all three. If unsorted-branchy is not clearly \
              slower, read the assembly before concluding anything.");
}

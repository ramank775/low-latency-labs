//! syscalls: Lab 2.2. Chapter 1's stride walk (256 MB of u64, every page
//! touched, then for strides of 1, 2, 4, … 4,096 elements the sum of every
//! stride-th element) with two extra flags: `--log-pass` prints one line per
//! pass with `println!`, `--log-access` every 1,000th access. The counting
//! allocator is this crate's global allocator, so the allocation count is
//! printed for every pass. Count the system calls from outside:
//!
//!     strace -c -f ./target/release/syscalls
//!     perf trace -s ./target/release/syscalls
//!     perf stat -e raw_syscalls:sys_enter,page-faults,context-switches,cpu-migrations ./target/release/syscalls
//!
//! and again with `--log-pass` and with `--log-access`.

use ch02_os::counting::allocs;
use ch02_os::procfs::{faults, sep, Snapshot};
use ch02_os::{calibrate, expected_sum, now, parse_num, parse_size, place, print_platform, quantile,
              walk, walk_logged, How, Log, ToNs, STRIDES};
use std::io::{self, BufWriter, Write};
use std::process::exit;
use std::time::Instant;

const USAGE: &str = "\
usage: syscalls [--log-pass] [--log-access] [--format] [--buffered] [--quiet] [--size BYTES] [--passes N] [--core N]

Chapter 1's stride walk over --size bytes of u64 (default 256M): every page
touched once, then --passes passes (default 3) at each stride of 1, 2, 4, ...
4,096 elements. Nothing inside the plain walk asks the kernel for anything.
  --log-pass     println! one line at the end of every pass
  --log-access   println! one line every 1,000th access, from inside the walk
  --format       the two logging flags build each line with format! and
                 write_all instead: one allocation per line
  --buffered     the two logging flags write into a BufWriter around stdout,
                 flushed at the end: a few large writes instead of one per line
  --quiet        skip the platform header and the /proc snapshots, whose
                 reads (openat, read, close) would otherwise sit in strace's
                 table beside the walk's own calls
  --size BYTES   the array (256M, 1G, 64M, ...)
  --passes N     passes per stride
  --core N       pin with sched_setaffinity (or run under taskset -c N)

Count the calls from outside (strace and perf are not run by this binary):
  strace -c -f ./target/release/syscalls
  perf trace -s ./target/release/syscalls                     (root, or perf_event_paranoid -1)
  perf stat -e raw_syscalls:sys_enter,page-faults,context-switches,cpu-migrations ./target/release/syscalls
and then the same three with --log-pass and with --log-access, to a pipe
(| tail -40) and to the terminal. strace -e trace=mmap,munmap,brk,mremap shows
only the allocator's crossings.";

fn usage(msg: &str) -> ! {
    eprintln!("syscalls: {msg}\n{USAGE}");
    exit(2)
}

struct Pass { stride: usize, ns: u64, allocs: u64, sum: u64 }

fn main() {
    ch02_os::default_sigpipe();
    let (mut log_pass, mut log_access, mut format, mut buffered, mut quiet) = (false, false, false, false, false);
    let (mut size, mut passes, mut core) = (256usize << 20, 3usize, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--log-pass" => log_pass = true,
            "--log-access" => log_access = true,
            "--format" => format = true,
            "--buffered" => buffered = true,
            "--quiet" => quiet = true,
            "--size" => size = parse_size(&val()).filter(|&s| s >= 8 << 10).unwrap_or_else(|| usage("--size: at least 8K, as 256M, 1G or bytes")),
            "--passes" => passes = parse_num(&val()).filter(|&n| n > 0).unwrap_or_else(|| usage("--passes: a number above 0")) as usize,
            "--core" => core = Some(parse_num(&val()).unwrap_or_else(|| usage("--core: a core number")) as usize),
            "-h" | "--help" => { println!("{USAGE}"); return; }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    let wall = Instant::now();
    let n = size / 8;
    let variant = match (log_pass, log_access) {
        (false, false) => "plain".to_string(),
        (p, a) => {
            let what = [(p, "--log-pass"), (a, "--log-access")].iter().filter(|x| x.0).map(|x| x.1).collect::<Vec<_>>().join(" ");
            let how = match (format, buffered) { (false, false) => "println!", (true, false) => "format! + write_all",
                                                 (false, true) => "writeln! into a BufWriter", (true, true) => "format! into a BufWriter" };
            format!("{what}, each line by {how}")
        }
    };
    println!("syscalls: Chapter 1's stride walk over {} MB of u64, {passes} passes per stride; variant: {variant} (Lab 2.2)", size >> 20);
    println!("Predict first (the chapter's prediction, before you measure): the plain walk makes a few dozen system calls \
              in total (the mmap for the array, a handful of writes for the output, exit) and none from inside the loop; \
              page-faults {} ({} MB over 4 KB) plus a few, all on the first pass. --log-pass adds one write per pass, trivial. \
              --log-access adds a write every 1,000 accesses: 1 µs of work and 2-5 µs of write, so the walk runs 3-6x slower, \
              more to a terminal than to a pipe, and a further 10-50x under strace, where the plain walk is barely affected. \
              The allocation count does not move in any variant; with --format it rises by one per line.",
             sep((size / 4096) as u64), size >> 20);
    if !quiet { print_platform(); }
    let p = place(core);
    println!("core: {}", p.note);
    let tpn = calibrate();
    let ns = ToNs::new(tpn);

    let out: Box<dyn Write> = if buffered { Box::new(BufWriter::with_capacity(1 << 16, io::stdout())) } else { Box::new(io::stdout()) };
    let how = match (format, buffered) { (true, _) => How::Format, (false, true) => How::Write, (false, false) => How::Println };
    let mut log = Log::new(how, out);
    let mut rows: Vec<Pass> = Vec::with_capacity(STRIDES.len() * passes);  // every row before the walk: no growth inside it
    println!();
    io::stdout().flush().expect("flush stdout");

    let before = if quiet { Snapshot::default() } else { Snapshot::take() };
    let (f0, _) = faults();
    let a0 = allocs();
    let t0 = now();
    let mut a = vec![0u64; n];            // one mmap: addresses, not frames
    let t_alloc = ns.of(now() - t0);
    let (f1, _) = faults();
    let t1 = now();
    for (i, x) in a.iter_mut().enumerate() { *x = i as u64; }   // touch every page: here they fault
    let t_touch = ns.of(now() - t1);
    let (f2, _) = faults();
    let a1 = allocs();

    let walk_start = now();
    let mut pass = 0;
    for &stride in &STRIDES {
        for _ in 0..passes {
            let aa = allocs();
            let t = now();
            let sum = if log_access { walk_logged(&a, stride, pass, &mut log) } else { walk(&a, stride) };
            let dt = ns.of(now() - t);
            let ab = allocs();
            if log_pass { log.pass(pass, stride, sum, dt); }
            rows.push(Pass { stride, ns: dt, allocs: ab - aa, sum });
            pass += 1;
        }
    }
    log.flush();
    let walk_ns = ns.of(now() - walk_start);
    let a2 = allocs();
    let (f3, _) = faults();
    let after = if quiet { Snapshot::default() } else { Snapshot::take() };

    println!();
    println!("vec![0u64; {}]: {} ns, {} faults, {} allocation (an mmap: the frames come later)",
             sep(n as u64), sep(t_alloc), sep(f1 - f0), a1 - a0);
    println!("touch, one write per element: {:.1} ms, {} faults ({} expected: one per 4 KB page)",
             t_touch as f64 / 1e6, sep(f2 - f1), sep((size / 4096) as u64));
    println!("{:>6} {:>8} {:>3} {:>10} {:>10} {:>10} {:>12} {:>7}",
             "stride", "bytes", "n", "p50 ms", "p99 ms", "max ms", "ns/access", "allocs");
    let mut bad = 0;
    for &stride in &STRIDES {
        let mut t: Vec<u64> = rows.iter().filter(|r| r.stride == stride).map(|r| r.ns).collect();
        t.sort_unstable();
        let al: u64 = rows.iter().filter(|r| r.stride == stride).map(|r| r.allocs).sum();
        bad += rows.iter().filter(|r| r.stride == stride && r.sum != expected_sum(n, stride)).count();
        let accesses = n.div_ceil(stride) as f64;
        println!("{:>6} {:>8} {:>3} {:>10.3} {:>10.3} {:>10.3} {:>12.2} {:>7}",
                 stride, stride * 8, t.len(), quantile(&t, 0.5) as f64 / 1e6, quantile(&t, 0.99) as f64 / 1e6,
                 quantile(&t, 1.0) as f64 / 1e6, quantile(&t, 0.5) as f64 / accesses, al);
    }
    println!("walk: {} passes in {:.1} ms, logging included; {} log lines ({})", rows.len(), walk_ns as f64 / 1e6, sep(log.lines),
             if buffered { "a few large writes, from the BufWriter" } else { "one write(2) each" });
    println!("faults during the walk: {} (the array was touched before it{})", sep(f3 - f2),
             if buffered { "; the BufWriter's buffer faults in as it first fills" } else { "" });
    println!("allocations during the walk: {} (println! allocates nothing; format! one per line)", a2 - a1);
    println!("sums: {}", if bad == 0 { "all as expected".to_string() } else { format!("{bad} WRONG") });
    println!("process: {:.1} ms from start to here", wall.elapsed().as_secs_f64() * 1e3);
    if !quiet {
        println!("deltas over the walk:");
        before.report(&after, &p.cores(), walk_ns as f64 / 1e9);
    }
    println!("Write down: the syscall counts, the slowdown of each logging variant, and the slowdown strace adds to each.");
    drop(a);
}

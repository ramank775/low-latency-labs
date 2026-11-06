//! first_touch: Lab 2.3. Maps 1 GB anonymously and times three passes: one
//! store per 4 KB page (the first touch), the same pass again, and a full
//! sequential write of the gigabyte.
//!
//!     first_touch [--populate] [--thp] [--hugetlb] [--mlockall] [--size BYTES] [--runs N] [--core N]
//!
//! Run each variant under `perf stat -e page-faults,dTLB-store-misses`. The
//! binary prints the thread's own fault counts (getrusage) beside the times.

use ch02_os::mem::{self, MapFlags, HUGE, PAGE};
use ch02_os::procfs::{faults, kb_field, read_trim, sep, Snapshot};
use ch02_os::{calibrate, full_write, new_hist, now, parse_num, parse_size, place, print_platform, quantile,
              store_per_page, store_per_page_timed, summary, ToNs};
use std::process::exit;
use std::time::Instant;

const USAGE: &str = "\
usage: first_touch [--populate] [--thp] [--hugetlb] [--mlockall] [--size BYTES] [--runs N] [--core N]

Maps --size bytes (default 1G) anonymously and times three passes: one store
per 4 KB page, the same pass again, and a full sequential write. Each run is a
fresh mapping; --runs (default 3) runs, then one more with the clock read around
every store, for the per-page distribution.
  --populate   add MAP_POPULATE to the mmap, and time the mmap
  --thp        madvise(MADV_HUGEPAGE) on the region (transparent_hugepage/enabled
               must be madvise or always)
  --hugetlb    map with MAP_HUGETLB (2 MB pages from the reserved pool: 512 for 1 GB,
               reserved first as root: echo 512 > /proc/sys/vm/nr_hugepages)
  --mlockall   mlockall(MCL_CURRENT | MCL_FUTURE) before the mmap (needs
               ulimit -l unlimited or CAP_IPC_LOCK)
  --size BYTES the region: 1G, 256M, ...
  --runs N     runs per variant
  --core N     pin with sched_setaffinity (or run under taskset -c N)

A refused --hugetlb or --mlockall is reported and the run continues without it,
labelled. The chapter's measurement, for each variant:
  perf stat -e page-faults,dTLB-store-misses ./target/release/first_touch
  perf stat -e page-faults,dTLB-store-misses ./target/release/first_touch --populate
  perf stat -e page-faults,dTLB-store-misses ./target/release/first_touch --thp
  perf stat -e page-faults,dTLB-store-misses ./target/release/first_touch --hugetlb
  perf stat -e page-faults,dTLB-store-misses ./target/release/first_touch --mlockall
perf's page-faults counts only faults a store raised; the getrusage counts printed
here also include the ones MAP_POPULATE and mlockall take inside the kernel.";

fn usage(msg: &str) -> ! {
    eprintln!("first_touch: {msg}\n{USAGE}");
    exit(2)
}

#[derive(Default)]
struct Pass { ns: Vec<u64>, faults: Vec<u64> }

impl Pass {
    fn push(&mut self, ns: u64, faults: u64) { self.ns.push(ns); self.faults.push(faults); }

    fn row(&mut self, name: &str, pages: usize, bytes: usize) {
        self.ns.sort_unstable();
        self.faults.sort_unstable();
        let p50 = quantile(&self.ns, 0.5);
        let rate = if bytes > 0 && p50 > 0 { format!("{:>8.1} GB/s", bytes as f64 / p50 as f64) } else { String::new() };
        let per_page = if pages > 0 { format!("{:>9.1}", p50 as f64 / pages as f64) } else { format!("{:>9}", "-") };
        println!("{name:<18} {:>3} {:>10.3} {:>10.3} {:>10.3} {:>10} {per_page} {rate}",
                 self.ns.len(), p50 as f64 / 1e6, quantile(&self.ns, 0.99) as f64 / 1e6, quantile(&self.ns, 1.0) as f64 / 1e6,
                 sep(quantile(&self.faults, 0.5)));
    }
}

fn timed(f: impl FnOnce()) -> (u64, u64) {
    let (f0, _) = faults();
    let t0 = now();
    f();
    let t1 = now();
    let (f1, _) = faults();
    (t1 - t0, f1 - f0)
}

fn main() {
    ch02_os::default_sigpipe();
    let (mut flags, mut thp, mut mlockall) = (MapFlags::default(), false, false);
    let (mut size, mut runs, mut core) = (1usize << 30, 3usize, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--populate" => flags.populate = true,
            "--thp" => thp = true,
            "--hugetlb" => flags.hugetlb = true,
            "--mlockall" => mlockall = true,
            "--size" => size = parse_size(&val()).filter(|&s| s >= HUGE).unwrap_or_else(|| usage("--size: at least 2M, as 1G, 256M or bytes")),
            "--runs" => runs = parse_num(&val()).filter(|&n| n > 0).unwrap_or_else(|| usage("--runs: a number above 0")) as usize,
            "--core" => core = Some(parse_num(&val()).unwrap_or_else(|| usage("--core: a core number")) as usize),
            "-h" | "--help" => { println!("{USAGE}"); return; }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    flags.align_2m = thp;   // THP backs only whole, aligned 2 MB blocks
    let size = size.div_ceil(HUGE) * HUGE;
    let pages = size / PAGE;
    let mut variant: Vec<&str> = Vec::new();
    if flags.populate { variant.push("--populate"); }
    if thp { variant.push("--thp"); }
    if flags.hugetlb { variant.push("--hugetlb"); }
    if mlockall { variant.push("--mlockall"); }
    let label = if variant.is_empty() { "plain".to_string() } else { variant.join(" ") };

    println!("first_touch: {} MB anonymous, {} pages of 4 KB, variant {label}, {runs} runs (Lab 2.3)", size >> 20, sep(pages as u64));
    let scale = size as f64 / (1u64 << 30) as f64;
    println!("Predict first (the chapter's prediction for 1 GB, scaled to {} MB): plain pass 1 is {} faults at 0.5-1 µs, \
              {:.0}-{:.0} ms; pass 2 stores to resident pages, a TLB and cache miss each but overlapped, {:.1}-{:.0} ms, \
              20-30 ns a page, 20-50x faster; the full write is bandwidth, 10-20 GB/s, {:.0}-{:.0} ms.",
             size >> 20, sep(pages as u64), 130.0 * scale, 260.0 * scale, 5.0 * scale, 10.0 * scale, 50.0 * scale, 100.0 * scale);
    if flags.populate { println!("Predict first, --populate: the faults move into the mmap, somewhat less than the plain pass 1, and pass 1 then equals pass 2."); }
    if thp || flags.hugetlb {
        println!("Predict first, huge pages: {} faults, each zeroing 2 MB in 100-200 µs, so pass 1 is {:.0}-{:.0} ms of zeroing \
                  bandwidth, and pass 2's dTLB-store-misses fall by about 512x.", sep((size / HUGE) as u64), 50.0 * scale, 100.0 * scale);
    }
    if mlockall { println!("Predict first, --mlockall: the cost moves into the mmap like --populate, and Locked in smaps_rollup equals the region."); }
    print_platform();
    let p = place(core);
    println!("core: {}", p.note);
    println!("locked-memory limit: {}; huge-page pool: {} total, {} free",
             mem::memlock_limit(), kb_field("/proc/meminfo", "HugePages_Total").unwrap_or(0), kb_field("/proc/meminfo", "HugePages_Free").unwrap_or(0));
    let tpn = calibrate();
    let ns = ToNs::new(tpn);

    if mlockall {
        match mem::lock_all() {
            Ok(()) => println!("mlockall(MCL_CURRENT | MCL_FUTURE): ok"),
            Err(e) => { println!("warning: {e}: this run is NOT locked"); mlockall = false; }
        }
    }

    let (mut h1, mut h2) = (new_hist(), new_hist());
    let (mut m, mut p1, mut p2, mut p3) = (Pass::default(), Pass::default(), Pass::default(), Pass::default());
    let (mut huge_kb, mut locked_kb) = (None, None);
    let before = Snapshot::take();
    let started = Instant::now();
    for run in 0..=runs {
        let per_page = run == runs;     // the last run: the clock around every store
        let (mut region, t, f) = loop {
            let (f0, _) = faults();
            let t0 = now();
            let r = mem::map(size, flags);
            let t1 = now();
            let (f1, _) = faults();
            match r {
                Ok(r) => break (r, t1 - t0, f1 - f0),
                // degrade, labelled: the same run without what was refused
                Err(e) if flags.hugetlb => {
                    println!("warning: {e}\nwarning: continuing with 4 KB pages: these numbers are NOT --hugetlb");
                    flags.hugetlb = false;
                }
                Err(e) if mlockall => {
                    println!("warning: {e}\nwarning: munlockall and continuing: these numbers are NOT --mlockall");
                    mem::unlock_all();
                    mlockall = false;
                }
                Err(e) => { println!("first_touch: {e}"); exit(1) }
            }
        };
        if thp {
            let a = mem::advise_huge(&region);
            if run == 0 { println!("{}", a.unwrap_or_else(|e| format!("warning: {e}"))); }
        }
        let buf = region.bytes_mut();
        if per_page {
            store_per_page_timed(buf, &mut h1, ns);
            store_per_page_timed(buf, &mut h2, ns);
        } else {
            m.push(ns.of(t), f);
            let (t, f) = timed(|| store_per_page(buf));
            p1.push(ns.of(t), f);
            let (t, f) = timed(|| store_per_page(buf));
            p2.push(ns.of(t), f);
            let (t, f) = timed(|| full_write(buf));
            p3.push(ns.of(t), f);
            if run == runs - 1 {
                huge_kb = kb_field("/proc/self/smaps_rollup", "AnonHugePages");
                locked_kb = kb_field("/proc/self/smaps_rollup", "Locked");
            }
        }
        drop(region);
    }
    let elapsed = started.elapsed().as_secs_f64();
    let after = Snapshot::take();

    println!();
    let final_label = {
        let mut v = Vec::new();
        if flags.populate { v.push("--populate"); }
        if thp { v.push("--thp"); }
        if flags.hugetlb { v.push("--hugetlb"); }
        if mlockall { v.push("--mlockall"); }
        if v.is_empty() { "plain".to_string() } else { v.join(" ") }
    };
    println!("variant measured: {final_label}");
    println!("{:<18} {:>3} {:>10} {:>10} {:>10} {:>10} {:>9}", "", "n", "p50 ms", "p99 ms", "max ms", "faults", "ns/page");
    m.row("mmap", 0, 0);
    p1.row("pass 1 first touch", pages, 0);
    p2.row("pass 2 again", pages, 0);
    p3.row("pass 3 full write", 0, size);
    let (a, b) = (quantile(&p1.ns, 0.5), quantile(&p2.ns, 0.5));
    if b > 0 { println!("pass 1 / pass 2 at p50: {:.1}x (the cost of faulting)", a as f64 / b as f64); }
    println!("faults: getrusage minor faults of this thread, p50 over the runs (perf's page-faults leaves out the kernel's own, \
              taken inside MAP_POPULATE and mlockall)");
    println!("per store, one extra run with now() around each (a clock pair, ~{} ns, is inside every sample):",
             sep(ch02_os::clock_pair_p50(10_000, ns)));
    println!("  pass 1: {}", summary(&h1));
    println!("  pass 2: {}", summary(&h2));
    println!("smaps_rollup after the passes: AnonHugePages {} kB, Locked {} kB (region {} kB); THP enabled: {}",
             huge_kb.map_or("?".into(), sep), locked_kb.map_or("?".into(), sep), sep((size / 1024) as u64),
             read_trim("/sys/kernel/mm/transparent_hugepage/enabled").unwrap_or_else(|| "absent".into()));
    println!("deltas over all runs:");
    before.report(&after, &p.cores(), elapsed);
    println!("Write down: the fault count and the three pass times for every variant, and the cost per 4 KB page implied by each.");
}

//! Chapter 2, "The platform underneath": the operating system a hot loop
//! runs on, measured. The chapter holds the reasons; this crate holds the
//! code for its lab:
//!
//! - `jitter`      Lab 2.1: `rdtsc` back to back, every gap into a histogram
//! - `syscalls`    Lab 2.2: Chapter 1's stride walk, with and without logging
//! - `first_touch` Lab 2.3: the cost of a page the first time and the second
//! - `wake`        Lab 2.4: spin, park and sleep between two pinned threads
//!
//! The library holds the chapter's two code blocks, the counting allocator
//! (`counting`, the global allocator of everything in this crate) and the spin
//! loop from "Not waiting" (`poll`, over anything `TryPop`), and what the four binaries share: the
//! clock, the histograms, pinning, the platform header, and the /proc reads.

pub mod counting;
pub mod mem;
pub mod procfs;

use core::arch::x86_64::{_mm_lfence, _rdtsc};
use std::hint::{black_box, spin_loop};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering::{Acquire, Release}};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

use hdrhistogram::Histogram;

use crate::counting::allocs;
use crate::procfs::{read_trim, ThreadCounters};

// ---------------------------------------------------------------- the clock

/// The TSC, read once every earlier instruction has finished.
#[inline(always)]
pub fn now() -> u64 {
    // SAFETY: lfence and rdtsc exist on every x86_64 part and touch no memory.
    unsafe {
        _mm_lfence();   // wait for earlier instructions: do not read early
        _rdtsc()
    }
}

/// The TSC with no fence: Lab 2.1's read. Both reads of a pair are the same
/// instruction, so the reordering is a few cycles and does not matter there.
#[inline(always)]
pub fn now_unfenced() -> u64 {
    // SAFETY: rdtsc exists on every x86_64 part and touches no memory.
    unsafe { _rdtsc() }
}

/// TSC ticks per nanosecond, measured once at startup against CLOCK_MONOTONIC.
pub fn calibrate() -> f64 {
    let (i0, t0) = (Instant::now(), now());
    while i0.elapsed() < Duration::from_millis(200) {}  // spin for 200 ms
    let (i1, t1) = (Instant::now(), now());
    (t1 - t0) as f64 / (i1 - i0).as_nanos() as f64
}

/// Ticks to nanoseconds in 32.32 fixed point: one multiply and a shift, cheap
/// enough to sit inside Lab 2.1's loop.
#[derive(Clone, Copy, Debug)]
pub struct ToNs { mul: u64 }

impl ToNs {
    pub fn new(ticks_per_ns: f64) -> ToNs { ToNs { mul: (4_294_967_296.0 / ticks_per_ns) as u64 } }

    #[inline(always)]
    pub fn of(self, ticks: u64) -> u64 { ((ticks as u128 * self.mul as u128) >> 32) as u64 }
}

/// The p50 of back-to-back `now()` pairs in ns: the clock's own share of every interval it closes.
pub fn clock_pair_p50(pairs: usize, ns: ToNs) -> u64 {
    let mut h = new_hist();
    for _ in 0..pairs {
        let a = now();
        let b = now();
        h.saturating_record(ns.of(b - a));
    }
    h.value_at_quantile(0.50)
}

// ---------------------------------------------------------------- histograms

/// 1 ns to 1 s at 3 significant figures, every counter already written once,
/// so the first sample in a new range does not take a page fault.
pub fn new_hist() -> Histogram<u64> {
    let mut h = Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).expect("valid bounds");
    h.reset();      // writes every counter: the pages are faulted in now, not in the loop
    h
}

/// Samples strictly above `ns`.
pub fn above(h: &Histogram<u64>, ns: u64) -> u64 {
    if ns >= h.max() { return 0; }
    h.count_between(h.highest_equivalent(ns) + 1, u64::MAX)
}

/// `n … p50 … p99 … p99.9 … p99.99 … max … ns`: never a mean.
pub fn summary(h: &Histogram<u64>) -> String {
    use procfs::sep;
    format!("n {} p50 {} p99 {} p99.9 {} p99.99 {} max {} ns",
            sep(h.len()), sep(h.value_at_quantile(0.50)), sep(h.value_at_quantile(0.99)),
            sep(h.value_at_quantile(0.999)), sep(h.value_at_quantile(0.9999)), sep(h.max()))
}

/// The nearest-rank quantile of a sorted slice: for the few-sample tables
/// (three runs, five passes) where a histogram would be ceremony.
pub fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() { return 0; }
    let rank = (q * sorted.len() as f64).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

// ---------------------------------------------------------------- cores

/// The cores this thread's affinity mask allows.
pub fn allowed_cores() -> Vec<usize> {
    // SAFETY: an all-zero cpu_set_t is the empty set, and sched_getaffinity writes only into it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: `set` is a live local of exactly the size passed.
    let rc = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 { return Vec::new(); }
    // SAFETY: CPU_ISSET reads the set just filled, for indices below its capacity.
    (0..libc::CPU_SETSIZE as usize).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).collect()
}

/// Shrink the calling thread's affinity mask to one core.
pub fn pin_to(core: usize) -> Result<(), String> {
    if core >= libc::CPU_SETSIZE as usize { return Err(format!("core {core} is beyond CPU_SETSIZE")); }
    let allowed = allowed_cores();
    // SAFETY: an all-zero cpu_set_t is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: core < CPU_SETSIZE, checked above.
    unsafe { libc::CPU_SET(core, &mut set) };
    // SAFETY: `set` is a live local of exactly the size passed; 0 is the calling thread.
    if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) } == 0 {
        return Ok(());
    }
    Err(format!("sched_setaffinity to core {core} refused: {} (this process may use cores {})",
                std::io::Error::last_os_error(), cpu_list(&allowed)))
}

/// The core the calling thread is on right now.
pub fn current_cpu() -> usize {
    // SAFETY: sched_getcpu takes nothing and touches no memory of ours.
    let c = unsafe { libc::sched_getcpu() };
    c.max(0) as usize
}

/// `[0, 1, 2, 5]` as "0-2,5".
pub fn cpu_list(cores: &[usize]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < cores.len() {
        let mut j = i;
        while j + 1 < cores.len() && cores[j + 1] == cores[j] + 1 { j += 1; }
        out.push(if j == i { cores[i].to_string() } else { format!("{}-{}", cores[i], cores[j]) });
        i = j + 1;
    }
    if out.is_empty() { "none".into() } else { out.join(",") }
}

/// "3-7,9" as `[3, 4, 5, 6, 7, 9]`; empty or "(null)" as none.
pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut v = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) { v.extend(a..=b) },
            None => if let Ok(a) = part.parse() { v.push(a) },
        }
    }
    v
}

/// Where a run's thread is allowed to be, and how it got there.
#[derive(Clone, Debug)]
pub struct Placement {
    pub pinned: Option<usize>,
    pub allowed: Vec<usize>,
    pub note: String,
}

impl Placement {
    /// The columns of /proc/interrupts that belong to this run.
    pub fn cores(&self) -> Vec<usize> {
        match self.pinned { Some(c) => vec![c], None => self.allowed.clone() }
    }

    pub fn isolated(&self) -> bool {
        let iso = parse_cpu_list(&read_trim("/sys/devices/system/cpu/isolated").unwrap_or_default());
        let nohz = parse_cpu_list(&read_trim("/sys/devices/system/cpu/nohz_full").unwrap_or_default());
        self.pinned.is_some_and(|c| iso.contains(&c) || nohz.contains(&c))
    }
}

/// Pin to `core` if one is given; otherwise report the mask the process
/// started with (a one-core mask is `taskset -c N`). A refused pin is a
/// warning and an unpinned run, labelled as one.
pub fn place(core: Option<usize>) -> Placement {
    let allowed = allowed_cores();
    match core {
        Some(c) => match pin_to(c) {
            Ok(()) => Placement { pinned: Some(c), allowed, note: format!("pinned to core {c} with sched_setaffinity (--core {c})") },
            Err(e) => {
                eprintln!("warning: {e}: this run is UNPINNED");
                let note = format!("UNPINNED: --core {c} was refused; may run on cores {}", cpu_list(&allowed));
                Placement { pinned: None, allowed, note }
            }
        },
        None if allowed.len() == 1 => {
            let c = allowed[0];
            Placement { pinned: Some(c), allowed, note: format!("pinned to core {c} by the mask it started with (taskset -c {c})") }
        }
        None => {
            let note = format!("unpinned: may run on cores {}, started on {}", cpu_list(&allowed), current_cpu());
            Placement { pinned: None, allowed, note }
        }
    }
}

// ---------------------------------------------------------------- the platform

/// What the chapter's rules say to record with every run: the kernel, the
/// command line, bare metal or a VM, and what the box isolates.
pub fn print_platform() {
    let release = read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into());
    println!("kernel: {release}");
    println!("cmdline: {}", read_trim("/proc/cmdline").unwrap_or_default());
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let guest = cpuinfo.lines().find(|l| l.starts_with("flags")).is_some_and(|l| l.split_whitespace().any(|f| f == "hypervisor"));
    let model = cpuinfo.lines().find_map(|l| l.strip_prefix("model name")).map(|s| s.trim_start_matches([' ', '\t', ':'])).unwrap_or("unknown");
    println!("machine: {}", if guest {
        "a VM guest (cpuinfo has the hypervisor flag): the host's tick, scheduler and interrupts are invisible here except as steal"
    } else {
        "bare metal (no hypervisor flag in cpuinfo)"
    });
    let online = read_trim("/sys/devices/system/cpu/online").unwrap_or_default();
    println!("cpu: {model}; online {online}");
    let hz = std::fs::read_to_string(format!("/boot/config-{release}")).ok()
        .and_then(|c| c.lines().find_map(|l| l.strip_prefix("CONFIG_HZ=").map(str::to_string)))
        .unwrap_or_else(|| format!("unknown (no /boot/config-{release}; the LOC delta of a busy core gives it)"));
    let none = |s: Option<String>| match s { Some(s) if !s.is_empty() && s != "(null)" => s, Some(_) => "none".into(), None => "not supported by this kernel".into() };
    println!("clocksource: {}; CONFIG_HZ: {hz}",
             read_trim("/sys/devices/system/clocksource/clocksource0/current_clocksource").unwrap_or_default());
    let thp = |f: &str| read_trim(&format!("/sys/kernel/mm/transparent_hugepage/{f}"))
        .map(|s| procfs::bracketed(&s).unwrap_or("?").to_string()).unwrap_or_else(|| "absent".into());
    println!("isolated: {}; nohz_full: {}; THP enabled={} defrag={}",
             none(read_trim("/sys/devices/system/cpu/isolated")),
             none(read_trim("/sys/devices/system/cpu/nohz_full")), thp("enabled"), thp("defrag"));
}

// ---------------------------------------------------------------- arguments

/// Die quietly on a closed pipe (`| head`), as a C tool does, instead of
/// panicking in the middle of a `println!`.
pub fn default_sigpipe() {
    // SAFETY: restoring a signal's default disposition touches no memory of ours.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
}

/// "1G", "512M", "64K", "1GB", "1GiB" or a plain byte count.
pub fn parse_size(s: &str) -> Option<usize> {
    let t = s.trim().to_ascii_uppercase().replace('_', "");
    let t = t.trim_end_matches("IB").trim_end_matches('B');
    let (num, mul) = match t.chars().last()? {
        'K' => (&t[..t.len() - 1], 1usize << 10),
        'M' => (&t[..t.len() - 1], 1 << 20),
        'G' => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    num.parse::<usize>().ok()?.checked_mul(mul).filter(|&n| n > 0)
}

/// A plain number, `_` separators allowed.
pub fn parse_num(s: &str) -> Option<u64> {
    s.replace(['_', ','], "").parse().ok()
}

// ---------------------------------------------------------------- Lab 2.1: the jitter loop

/// Read `rdtsc` back to back until `until`, recording every gap in ns.
/// Nothing allocated: `h` comes from `new_hist`, whose counters are all
/// resident. Returns the gaps that came out negative, which only a move to a
/// core whose counter disagrees can produce; they are counted, not recorded.
#[inline(never)]
pub fn jitter(h: &mut Histogram<u64>, until: u64, ns: ToNs) -> u64 {
    let mut backwards = 0;
    let mut prev = now_unfenced();
    loop {
        let t = now_unfenced();
        if t >= prev { h.saturating_record(ns.of(t - prev)); } else { backwards += 1; }
        prev = t;
        if t >= until { return backwards; }
    }
}

// ---------------------------------------------------------------- Lab 2.2: Chapter 1's stride walk

/// Strides of 1, 2, 4, … 4,096 elements.
pub const STRIDES: [usize; 13] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];

/// The sum of every `stride`-th element: the plain walk, no calls inside.
#[inline(never)]
pub fn walk(a: &[u64], stride: usize) -> u64 {
    a.iter().step_by(stride).fold(0u64, |s, &x| s.wrapping_add(x))
}

/// The same walk, writing one line every 1,000th access.
#[inline(never)]
pub fn walk_logged<W: Write>(a: &[u64], stride: usize, pass: usize, log: &mut Log<W>) -> u64 {
    let mut sum = 0u64;
    let mut left = 1000;
    for (i, &x) in a.iter().enumerate().step_by(stride) {
        sum = sum.wrapping_add(x);
        left -= 1;
        if left == 0 { left = 1000; log.access(pass, stride, i, x); }
    }
    sum
}

/// What `walk` returns over `a[i] = i`: stride × m(m−1)/2, m the accesses.
pub fn expected_sum(len: usize, stride: usize) -> u64 {
    let m = len.div_ceil(stride) as u64;
    (stride as u64).wrapping_mul(m * m.saturating_sub(1) / 2)
}

/// How a log line reaches the kernel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    Println, // the chapter's version: stdout's line buffer, one write(2) per line
    Format,  // format! into a String, then write_all: one allocation per line as well
    Write,   // writeln! into `out` directly (a BufWriter, for --buffered)
}

/// The logging variants' sink. `out` is unused by `How::Println`, which goes
/// through `println!` itself, the code the chapter names.
pub struct Log<W: Write> {
    pub how: How,
    pub out: W,
    pub lines: u64,
}

impl<W: Write> Log<W> {
    pub fn new(how: How, out: W) -> Log<W> { Log { how, out, lines: 0 } }

    /// `--log-access`: one line per 1,000 accesses, from inside the walk.
    #[inline(never)]
    pub fn access(&mut self, pass: usize, stride: usize, i: usize, x: u64) {
        match self.how {
            How::Println => println!("access {i} of stride {stride} pass {pass}: value {x}"),
            How::Format => {
                let line = format!("access {i} of stride {stride} pass {pass}: value {x}\n");
                self.out.write_all(line.as_bytes()).expect("write a log line");
            }
            How::Write => writeln!(self.out, "access {i} of stride {stride} pass {pass}: value {x}").expect("write a log line"),
        }
        self.lines += 1;
    }

    /// `--log-pass`: one line at the end of each pass.
    #[inline(never)]
    pub fn pass(&mut self, pass: usize, stride: usize, sum: u64, ns: u64) {
        match self.how {
            How::Println => println!("pass {pass} done: stride {stride}, sum {sum}, {ns} ns"),
            How::Format => {
                let line = format!("pass {pass} done: stride {stride}, sum {sum}, {ns} ns\n");
                self.out.write_all(line.as_bytes()).expect("write a log line");
            }
            How::Write => writeln!(self.out, "pass {pass} done: stride {stride}, sum {sum}, {ns} ns").expect("write a log line"),
        }
        self.lines += 1;
    }

    pub fn flush(&mut self) { self.out.flush().expect("flush the log"); }
}

// ---------------------------------------------------------------- Lab 2.3: the passes

/// One store per 4 KB page. The first time through, every one is a minor fault.
#[inline(never)]
pub fn store_per_page(buf: &mut [u8]) {
    let p = buf.as_mut_ptr();
    for off in (0..buf.len()).step_by(mem::PAGE) {
        // SAFETY: off < buf.len(), so the byte is inside the slice; volatile so no pass is elided.
        unsafe { p.add(off).write_volatile(1) };
    }
}

/// The same pass with `now()` around every store, each into `h`: the
/// distribution behind the total, at the price of a clock pair per page.
#[inline(never)]
pub fn store_per_page_timed(buf: &mut [u8], h: &mut Histogram<u64>, ns: ToNs) {
    let p = buf.as_mut_ptr();
    for off in (0..buf.len()).step_by(mem::PAGE) {
        let t0 = now();
        // SAFETY: as in store_per_page.
        unsafe { p.add(off).write_volatile(1) };
        let t1 = now();
        h.saturating_record(ns.of(t1 - t0));
    }
}

/// A full sequential write of the region: bandwidth, once the pages exist.
#[inline(never)]
pub fn full_write(buf: &mut [u8]) {
    buf.fill(2);
    black_box(buf);
}

// ---------------------------------------------------------------- "Not waiting": the spin loop

/// Anything the loop in "Not waiting" can poll: the data-structures
/// chapter's `Consumer` is one (`fn try_pop(&mut self) -> Option<T>`), and
/// so is a std channel's receiving end, which the tests use.
pub trait TryPop {
    type Item;
    fn try_pop(&mut self) -> Option<Self::Item>;
}

impl<T> TryPop for std::sync::mpsc::Receiver<T> {
    type Item = T;
    fn try_pop(&mut self) -> Option<T> { self.try_recv().ok() }
}

/// The loop printed in "Not waiting": poll, handle, and on an empty poll one
/// `pause`, never the kernel. It does not return; a handler that must stop
/// it unwinds (the tests use `std::panic::resume_unwind`).
pub fn poll<R: TryPop>(ring: &mut R, mut handle: impl FnMut(R::Item)) -> ! {
    loop {
        if let Some(msg) = ring.try_pop() {
            handle(msg);
        } else {
            std::hint::spin_loop(); // one `pause` per empty poll
        }
    }
}

// ---------------------------------------------------------------- Lab 2.4: wake latency

/// How the token waits for its turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Wait {
    Spin,  // an AtomicBool and spin_loop: never leaves user mode
    Park,  // thread::park and unpark: a futex each way
    Sleep, // poll the AtomicBool with thread::sleep(1 µs) between polls
}

impl Wait {
    pub fn name(self) -> &'static str {
        match self { Wait::Spin => "spin", Wait::Park => "park", Wait::Sleep => "sleep" }
    }
}

/// The token, on a pair of cache lines of its own so nothing else rides the hop.
#[repr(align(128))]
struct Token(AtomicBool);

/// One variant's result: round trips in ns as seen by the ping thread.
pub struct WakeRun {
    pub h: Histogram<u64>,
    pub ping_core: Option<usize>,       // None: unpinned
    pub pong_core: Option<usize>,
    pub notes: Vec<String>,             // refused pins and prctls
    pub allocs: u64,                    // allocations during the measured round trips
    pub slack_ns: [i64; 2],             // each thread's timer slack during the run
    pub ping_counters: [ThreadCounters; 2],
    pub pong_counters: [ThreadCounters; 2],
}

const MICRO: Duration = Duration::from_micros(1);

/// Two threads, on `cores[0]` (ping, the caller) and `cores[1]` (pong), pass
/// a token `warmup + iters` times; ping times each round trip.
pub fn ping_pong(wait: Wait, iters: u64, warmup: u64, cores: [Option<usize>; 2], slack: Option<u64>, ns: ToNs) -> WakeRun {
    let total = warmup + iters;
    let token = Arc::new(Token(AtomicBool::new(false)));
    let mut notes = Vec::new();
    let ping_core = cores[0].and_then(|c| pin_to(c).map(|_| c).map_err(|e| notes.push(format!("ping: {e}"))).ok());
    if let Some(s) = slack { if let Err(e) = mem::set_timer_slack(s) { notes.push(format!("ping: {e}")); } }
    let ping_thread = thread::current();
    let t = token.clone();
    let measured = Arc::new(Barrier::new(2));   // pong reads /proc, which allocates, only after ping has counted
    let m = measured.clone();
    let pong = thread::Builder::new().name("pong".into()).spawn(move || {
        let pinned = cores[1].map(|c| pin_to(c).map(|_| c));
        let slacked = slack.map(mem::set_timer_slack);
        let slack_now = mem::timer_slack();
        let before = ThreadCounters::read();
        let flag = &t.0;
        for _ in 0..total {
            match wait {
                Wait::Spin => while !flag.load(Acquire) { spin_loop() },
                Wait::Park => while !flag.load(Acquire) { thread::park() },
                Wait::Sleep => while !flag.load(Acquire) { thread::sleep(MICRO) },
            }
            flag.store(false, Release);
            if wait == Wait::Park { ping_thread.unpark(); }
        }
        m.wait();
        (pinned, slacked, slack_now, before, ThreadCounters::read())
    }).expect("spawn the pong thread");
    let pong_thread = pong.thread().clone();

    let mut h = new_hist();
    let ping_slack = mem::timer_slack();
    let before = ThreadCounters::read();
    let flag = &token.0;
    let mut a0 = allocs();
    for i in 0..total {
        if i == warmup { a0 = allocs(); }
        let t0 = now();
        flag.store(true, Release);
        match wait {
            Wait::Spin => while flag.load(Acquire) { spin_loop() },
            Wait::Park => { pong_thread.unpark(); while flag.load(Acquire) { thread::park() } }
            Wait::Sleep => while flag.load(Acquire) { thread::sleep(MICRO) },
        }
        let t1 = now();
        if i >= warmup { h.saturating_record(ns.of(t1 - t0)); }
    }
    let a1 = allocs();
    measured.wait();
    let after = ThreadCounters::read();
    let (pinned, slacked, pong_slack, pb, pa) = pong.join().expect("the pong thread panicked");
    let pong_core = match pinned {
        Some(Ok(c)) => Some(c),
        Some(Err(e)) => { notes.push(format!("pong: {e}")); None }
        None => None,
    };
    if let Some(Err(e)) = slacked { notes.push(format!("pong: {e}")); }
    WakeRun { h, ping_core, pong_core, notes, allocs: a1 - a0, slack_ns: [ping_slack, pong_slack],
              ping_counters: [before, after], pong_counters: [pb, pa] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_lists() {
        assert_eq!(parse_size("1G"), Some(1 << 30));
        assert_eq!(parse_size("256M"), Some(256 << 20));
        assert_eq!(parse_size("256MB"), Some(256 << 20));
        assert_eq!(parse_size("1GiB"), Some(1 << 30));
        assert_eq!(parse_size("4096"), Some(4096));
        assert_eq!(parse_size("0"), None);
        assert_eq!(parse_size("lots"), None);
        assert_eq!(parse_cpu_list("3-7,9"), vec![3, 4, 5, 6, 7, 9]);
        assert_eq!(parse_cpu_list("(null)"), Vec::<usize>::new());
        assert_eq!(cpu_list(&[0, 1, 2, 5, 7, 8]), "0-2,5,7-8");
        assert_eq!(cpu_list(&[]), "none");
    }

    #[test]
    fn quantiles_by_rank() {
        let v = [10, 20, 30];
        assert_eq!((quantile(&v, 0.5), quantile(&v, 0.99), quantile(&v, 1.0)), (20, 30, 30));
        assert_eq!(quantile(&[], 0.5), 0);
    }

    #[test]
    fn fixed_point_conversion() {
        let ns = ToNs::new(2.8);
        assert_eq!(ns.of(2_800), 999);          // 32.32 rounds down: under a nanosecond in a microsecond
        let ten_s = ns.of(28_000_000_000);
        assert!((9_999_999_990..=10_000_000_000).contains(&ten_s), "{ten_s}");   // and 3 ns in ten seconds
    }

    #[test]
    fn stride_walk_sums() {
        let a: Vec<u64> = (0..100_000).collect();
        for s in STRIDES { assert_eq!(walk(&a, s), expected_sum(a.len(), s), "stride {s}"); }
        let mut log = Log::new(How::Write, Vec::new());
        assert_eq!(walk_logged(&a, 1, 0, &mut log), expected_sum(a.len(), 1));
        assert_eq!(log.lines, 100);
        assert!(String::from_utf8(log.out).unwrap().starts_with("access 999 of stride 1 pass 0: value 999\n"));
    }

    #[test]
    fn above_counts_the_tail() {
        let mut h = new_hist();
        for v in [10, 10, 10, 1_500, 20_000, 300_000] { h.record(v).unwrap(); }
        assert_eq!((above(&h, 1_000), above(&h, 10_000), above(&h, 100_000), above(&h, 1_000_000)), (3, 2, 1, 0));
    }
}

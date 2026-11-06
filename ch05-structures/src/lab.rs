//! The lab's measuring kit, shared by this crate's bins and the book chapter's:
//! the fenced `rdtsc` clock and its calibration, the cost of reading it twice
//! back to back (subtracted from every interval), fixed-memory histograms
//! printed as p50, p99, p99.9 and max and never a mean, pinning to a core, the
//! kernel's fault and switch counts that name a max, and small flag parsers.

use core::arch::x86_64::{_mm_lfence, _rdtsc};
use hdrhistogram::Histogram;
use std::io::Write;
use std::process::exit;
use std::time::{Duration, Instant};

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

/// The TSC with no fence: for spinning until a tick, where a read taken a
/// little early costs one more turn of the loop and nothing else.
#[inline(always)]
pub fn now_unfenced() -> u64 {
    // SAFETY: as in now(): rdtsc exists on every x86_64 part and touches no memory.
    unsafe { _rdtsc() }
}

/// The clock as a run uses it: ticks per nanosecond, and the p50 of two reads
/// back to back, which every interval it closes contains once.
#[derive(Clone, Copy, Debug)]
pub struct Clock {
    pub ticks_per_ns: f64,
    pub pair_ticks: u64,
}

impl Clock {
    /// 200 ms against CLOCK_MONOTONIC, then 100,000 back-to-back pairs.
    pub fn new() -> Clock {
        let (i0, t0) = (Instant::now(), now());
        while i0.elapsed() < Duration::from_millis(200) {}  // spin for 200 ms
        let (i1, t1) = (Instant::now(), now());
        let ticks_per_ns = (t1 - t0) as f64 / (i1 - i0).as_nanos() as f64;
        let mut pairs: Vec<u64> = (0..100_000).map(|_| { let a = now(); let b = now(); b - a }).collect();
        pairs.sort_unstable();
        Clock { ticks_per_ns, pair_ticks: pairs[pairs.len() / 2] }
    }

    /// Ticks to nanoseconds: one multiply, after the timed region.
    #[inline(always)]
    pub fn ns(&self, ticks: u64) -> u64 { (ticks as f64 / self.ticks_per_ns) as u64 }

    /// Nanoseconds to ticks, for pacing.
    pub fn ticks(&self, ns: u64) -> u64 { (ns as f64 * self.ticks_per_ns) as u64 }

    /// Two reads back to back, in nanoseconds: what to subtract.
    pub fn pair_ns(&self) -> f64 { self.pair_ticks as f64 / self.ticks_per_ns }

    pub fn describe(&self) -> String {
        format!("TSC {:.3} GHz; two rdtsc reads back to back: p50 {} ticks, {:.1} ns, subtracted below",
                self.ticks_per_ns, self.pair_ticks, self.pair_ns())
    }
}

impl Default for Clock {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------- histograms

/// Before the run: every byte the histogram will ever use.
pub fn new_hist() -> Histogram<u64> {
    // 1 ns to 1 s at 3 significant figures: about 170 KB, so recording never allocates
    Histogram::<u64>::new_with_bounds(1, 1_000_000_000, 3).expect("valid bounds")
}

/// A histogram read back: each value is `(recorded - sub) / per`, in ns.
/// `per` turns a block's time into a per-item time; `sub` is the clock pair.
#[derive(Clone, Copy, Debug, Default)]
pub struct Summary { pub n: u64, pub p50: f64, pub p99: f64, pub p999: f64, pub max: f64 }

pub fn summarize(h: &Histogram<u64>, per: f64, sub: f64) -> Summary {
    let q = |v: u64| ((v as f64 - sub).max(0.0)) / per;
    Summary { n: h.len(), p50: q(h.value_at_quantile(0.50)), p99: q(h.value_at_quantile(0.99)),
              p999: q(h.value_at_quantile(0.999)), max: q(h.max()) }
}

/// A number of nanoseconds with as many decimals as it can bear.
pub fn fmt_ns(x: f64) -> String {
    if x >= 100.0 { format!("{x:.0}") } else if x >= 10.0 { format!("{x:.1}") } else { format!("{x:.2}") }
}

/// A count with thousands separators: 10,000,000.
pub fn fmt_count(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}

impl Summary {
    /// `p50 x p99 x p99.9 x max x ns (n samples)`.
    pub fn line(&self) -> String {
        format!("p50 {:>7} p99 {:>7} p99.9 {:>7} max {:>9} ns  (n {})",
                fmt_ns(self.p50), fmt_ns(self.p99), fmt_ns(self.p999), fmt_ns(self.max), fmt_count(self.n))
    }
}

// ---------------------------------------------------------------- the prediction

/// The line every lab binary starts with: the chapter's prediction, to be
/// written down before the numbers below it are read.
pub fn predict(lines: &[&str]) {
    println!("Predict first (the chapter's prediction; write yours down before reading on):");
    for l in lines { println!("  {l}"); }
    println!();
}

// ---------------------------------------------------------------- pinning

/// Pin the calling thread to `core` with `sched_setaffinity`.
pub fn pin(core: usize) -> Result<(), String> {
    if core >= libc::CPU_SETSIZE as usize {
        return Err(format!("core {core} is beyond CPU_SETSIZE"));
    }
    // SAFETY: a zeroed cpu_set_t is the empty set; CPU_SET writes one bit inside it,
    // and sched_setaffinity reads exactly size_of::<cpu_set_t>() bytes from it.
    let rc = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set)
    };
    if rc == 0 { Ok(()) } else { Err(format!("{}", std::io::Error::last_os_error())) }
}

/// Pin if asked; on refusal say so and run on, never labelling unpinned
/// numbers as pinned. Returns what to print.
pub fn pin_or_warn(who: &str, core: Option<usize>) -> String {
    match core {
        None => format!("{who}: unpinned (asked for none)"),
        Some(c) => match pin(c) {
            Ok(()) => format!("{who}: pinned to core {c}"),
            Err(e) => format!("{who}: NOT pinned: core {c} refused ({e}); this machine has {} online \
                               (lscpu -e); running unpinned, so every percentile above the p50 is \
                               partly the scheduler's", online_cpus()),
        },
    }
}

/// The online CPUs as the kernel lists them: "0-1".
pub fn online_cpus() -> String {
    std::fs::read_to_string("/sys/devices/system/cpu/online").map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "an unknown set".to_string())
}

/// One line about the machine: the CPU, the cores and the clock source.
pub fn machine() -> String {
    let model = std::fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name"))
                       .and_then(|l| l.split(':').nth(1)).map(|m| m.trim().to_string()))
        .unwrap_or_else(|| "unknown CPU".to_string());
    let cs = std::fs::read_to_string("/sys/devices/system/clocksource/clocksource0/current_clocksource")
        .map(|s| s.trim().to_string()).unwrap_or_else(|_| "?".to_string());
    format!("{model}; CPUs online {}; clocksource {cs}", online_cpus())
}

// ---------------------------------------------------------------- what else the machine did

/// The process's minor faults and context switches, from /proc: read before
/// and after a timed pass, they name what a max far above the p99 was.
#[derive(Clone, Copy, Debug, Default)]
pub struct Kernel { pub minflt: u64, pub majflt: u64, pub voluntary: u64, pub involuntary: u64 }

impl Kernel {
    pub fn read() -> Option<Kernel> {
        let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
        let rest = &stat[stat.rfind(')')? + 2..];          // after the command name
        let f: Vec<&str> = rest.split(' ').collect();      // field 3 is f[0]
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let field = |key: &str| status.lines().find(|l| l.starts_with(key))
            .and_then(|l| l[key.len()..].trim().parse().ok());
        Some(Kernel { minflt: f.get(7)?.parse().ok()?, majflt: f.get(9)?.parse().ok()?,
                      voluntary: field("voluntary_ctxt_switches:")?,
                      involuntary: field("nonvoluntary_ctxt_switches:")? })
    }

    pub fn since(&self, before: &Kernel) -> Kernel {
        Kernel { minflt: self.minflt - before.minflt, majflt: self.majflt - before.majflt,
                 voluntary: self.voluntary - before.voluntary, involuntary: self.involuntary - before.involuntary }
    }

    pub fn line(&self) -> String {
        format!("{} minor faults, {} major, {} voluntary and {} involuntary context switches",
                self.minflt, self.majflt, self.voluntary, self.involuntary)
    }
}

// ---------------------------------------------------------------- flags

/// Print the problem and the usage, and exit 2; `-h` prints and exits 0.
pub fn usage(msg: &str, help: &str) -> ! {
    if msg.is_empty() {
        let _ = writeln!(std::io::stdout(), "{help}");     // `| head` closing the pipe is not an error
        exit(0);
    }
    eprintln!("{msg}\n\n{help}");
    exit(2)
}

/// A count: 10000000, 10_000_000 or 10,000,000.
pub fn count(s: &str) -> Option<u64> {
    s.replace(['_', ','], "").parse().ok()
}

/// One core, or `none` for unpinned.
pub fn core(s: &str) -> Option<Option<usize>> {
    if s == "none" { Some(None) } else { s.parse().ok().map(Some) }
}

/// Two cores, `A,B`, or `none` for both unpinned.
pub fn cores(s: &str) -> Option<[Option<usize>; 2]> {
    if s == "none" { return Some([None, None]); }
    let (a, b) = s.split_once(',')?;
    Some([core(a)?, core(b)?])
}

/// The value after a flag, or the usage.
pub fn value(args: &mut impl Iterator<Item = String>, flag: &str, help: &str) -> String {
    args.next().unwrap_or_else(|| usage(&format!("{flag} needs a value"), help))
}

/// splitmix64: small, fast, the same numbers on every machine.
#[derive(Clone, Copy, Debug)]
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform below `n` (below 2^32), by a multiply and a shift, not a division.
    pub fn below(&mut self, n: u64) -> u64 { ((self.next_u64() >> 32) * n) >> 32 }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 { (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_subtract_then_divide() {
        let mut h = new_hist();
        for v in [1_000u64, 2_000, 3_000] { h.record(v).unwrap(); }
        let s = summarize(&h, 10.0, 100.0);
        assert_eq!(s.n, 3);
        assert!((s.p50 - 190.0).abs() < 1.0, "{s:?}");
        assert!((s.max - 290.0).abs() < 1.0, "{s:?}");
    }

    #[test]
    fn flags_parse() {
        assert_eq!(count("10_000,000"), Some(10_000_000));
        assert_eq!(cores("2,3"), Some([Some(2), Some(3)]));
        assert_eq!(cores("none"), Some([None, None]));
        assert_eq!(core("x"), None);
        assert_eq!(fmt_count(1_234_567), "1,234,567");
        assert_eq!(fmt_count(12), "12");
    }

    #[test]
    fn the_rng_stays_below_its_bound_and_repeats_from_a_seed() {
        let (mut a, mut b) = (Rng(7), Rng(7));
        for _ in 0..10_000 { let x = a.below(64); assert!(x < 64); assert_eq!(x, b.below(64)); }
    }

    #[test]
    fn the_clock_calibrates_near_the_kernels_rate() {
        let c = Clock::new();
        assert!(c.ticks_per_ns > 0.5 && c.ticks_per_ns < 10.0, "{c:?}");
        assert!(Kernel::read().is_some());
    }
}

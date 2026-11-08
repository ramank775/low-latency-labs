//! The memory-and-CPU chapter's code. First the types and functions its
//! sections print, as printed: `PriceLevel` and the three `Order` layouts
//! (1.4), `Padded` and `Counters` (1.5, with the cached-index `try_push` in
//! `ring`), `sum1` and `sum4` (1.6), the branch-free forms (1.7). Then what the
//! lab binaries share: the loop each lab times, the clock, pinning,
//! percentiles, the seeded generator, the huge-page switch and the flags.

use core::arch::x86_64::{_mm_clflush, _mm_lfence, _mm_mfence, _rdtsc};
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::arch::asm;
use std::ops::{Deref, DerefMut};
use std::process::exit;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Barrier;

pub use crossbeam_utils::CachePadded;

pub mod ring;

/// A cache line on every x86 server: 64 bytes.
pub const LINE: usize = 64;

// ================================================================ 1.4 data layout

#[derive(Clone, Copy)]
pub struct PriceLevel {
    pub price: u64,
    pub qty: u64,
    pub order_count: u32,
    pub last_update: u64,
}
// size_of::<PriceLevel>() == 32, align_of::<PriceLevel>() == 8
const _: () = assert!(size_of::<PriceLevel>() == 32 && align_of::<PriceLevel>() == 8);

#[repr(C)]
pub struct OrderC { pub is_bid: bool, pub price: u64, pub flags: u8, pub qty: u32, pub venue: u16, pub id: u64 }

#[repr(C)]
pub struct OrderSorted { pub price: u64, pub id: u64, pub qty: u32, pub venue: u16, pub is_bid: bool, pub flags: u8 }

pub struct OrderRust { pub is_bid: bool, pub price: u64, pub flags: u8, pub qty: u32, pub venue: u16, pub id: u64 }

// repr(C) layouts are fixed by the rules; OrderRust's 24 is the current compiler's choice (tested, not asserted)
const _: () = assert!(size_of::<OrderC>() == 40 && size_of::<OrderSorted>() == 24);

// ================================================================ 1.5 false sharing

#[derive(Default)]
#[repr(align(64))]
pub struct Padded<T>(pub T);

#[derive(Default)]
pub struct Counters {
    pub a: Padded<AtomicU64>, // bytes 0..64
    pub b: Padded<AtomicU64>, // bytes 64..128
}
// size_of::<Counters>() == 128
const _: () = assert!(size_of::<Counters>() == 128);

// So a padded value reads as the value: `shared.tail.load(..)` in `ring` is the chapter's line.
impl<T> Deref for Padded<T> {
    type Target = T;
    fn deref(&self) -> &T { &self.0 }
}

/// Lab 1.3's adjacent variant: two counters side by side in one struct, at
/// offsets 0 and 8 of one line, the line in the chapter's figure.
#[derive(Default)]
#[repr(C, align(64))]
pub struct Adjacent {
    pub a: AtomicU64, // bytes 0..8
    pub b: AtomicU64, // bytes 8..16: the same line
}

/// Lab 1.3's padded variant: each counter wrapped in `CachePadded`, which on
/// x86-64 is 128 bytes, because the adjacent-line prefetcher couples pairs of lines.
#[derive(Default)]
pub struct PaddedPair {
    pub a: CachePadded<AtomicU64>, // bytes 0..128
    pub b: CachePadded<AtomicU64>, // bytes 128..256
}

// ================================================================ 1.6 the pipeline

pub fn sum1(v: &[f64]) -> f64 {
    v.iter().sum() // acc = acc + v[i]: each add waits for the previous one
}

pub fn sum4(v: &[f64]) -> f64 {
    let mut acc = [0.0f64; 4];
    let mut chunks = v.chunks_exact(4);
    for c in &mut chunks {
        acc[0] += c[0]; acc[1] += c[1]; acc[2] += c[2]; acc[3] += c[3];
    }
    acc.iter().sum::<f64>() + chunks.remainder().iter().sum::<f64>()
}

// ================================================================ 1.7 branch prediction

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Side { Bid, Ask }

// The four forms of "Branch-free techniques in Rust", one function each, so
// each can be tested and found in the assembly (`objdump -d -M intel`).

#[allow(clippy::let_and_return)]
pub fn select_px(cond: bool, bid_px: u64, ask_px: u64) -> u64 {
    // Bitmask select: mask is all ones if cond is true, all zeros otherwise.
    let mask = (cond as u64).wrapping_neg();
    let px = (bid_px & mask) | (ask_px & !mask);
    px
}

pub fn count_above_128(bytes: &[u8]) -> u64 {
    let mut count = 0;
    for &b in bytes {
        // Count without a branch.
        count += u64::from(b > 128);
    }
    count
}

pub fn apply_fill(mut position: i64, side: Side, fill_qty: i64) -> i64 {
    // Lookup table instead of `if side == Bid { 1 } else { -1 }`.
    const SIGN: [i64; 2] = [1, -1];
    position += SIGN[side as usize] * fill_qty;
    position
}

#[allow(clippy::let_and_return)]
pub fn best_px(best: u64, px: u64) -> u64 {
    // A simple if-expression usually compiles to a conditional move.
    let best = if px > best { px } else { best };
    best
}

/// An empty `asm!` statement: no instruction is emitted, but the compiler must
/// assume it has an effect, so it may not run it early, copy it into both
/// arms, or vectorise the loop around it. Left alone, LLVM turns
/// `if b > 128 { sum += b }` into a `cmov` or vector code, the pitfall of Lab
/// 1.2, and all three variants would run at the same speed.
#[inline(always)]
pub fn keep() {
    // SAFETY: an empty template: no instruction, and no memory, register or flag touched.
    unsafe { asm!("", options(nomem, nostack, preserves_flags)) }
}

/// Lab 1.2, branchy: one conditional jump per byte, taken when b > 128.
#[inline(never)]
pub fn sum_branchy(bytes: &[u8]) -> u64 {
    let mut sum = 0u64;
    for &b in bytes {
        if b > 128 {
            sum += u64::from(b);
            keep(); // an effect in the arm: the arm cannot run speculatively, so the `if` stays a jump
        }
    }
    sum
}

/// Lab 1.2, the conditional-move rewrite: both values ready, one selected.
#[inline(never)]
pub fn sum_branchless(bytes: &[u8]) -> u64 {
    let mut sum = 0u64;
    for &b in bytes {
        sum += if b > 128 { u64::from(b) } else { 0 }; // a select: cmov, nothing to mispredict
        keep(); // every byte, unconditionally: the loop stays scalar, so this is the cmov form
    }
    sum
}

/// The same select with the compiler left free: it vectorises the loop (SSE2
/// by default; `-C target-cpu=native` for AVX2, section 1.9).
#[inline(never)]
pub fn sum_vector(bytes: &[u8]) -> u64 {
    bytes.iter().map(|&b| if b > 128 { u64::from(b) } else { 0 }).sum()
}

/// A sorted copy, untimed: 256 counts, then 256 runs. Counting, not comparing,
/// so the setup itself adds no branch misses to a `perf stat` of the run.
pub fn counting_sort(src: &[u8]) -> Vec<u8> {
    let mut count = [0usize; 256];
    for &b in src { count[b as usize] += 1; }
    let mut out = Vec::with_capacity(src.len());
    for (b, &n) in count.iter().enumerate() { out.resize(out.len() + n, b as u8); }
    out
}

// ================================================================ Lab 1.1 stride walk

/// The strides of Lab 1.1, in elements: 1, 2, 4, … 4,096.
pub const STRIDES: [usize; 13] = [1, 2, 4, 8, 16, 32, 64, 128, 256, 512, 1024, 2048, 4096];

/// Every stride-th element of `v`, summed: the addresses come from a counter,
/// so the core can have many of these loads in flight at once.
#[inline(never)]
pub fn stride_sum(v: &[u64], stride: usize) -> u64 {
    v.iter().step_by(stride).fold(0u64, |s, &x| s.wrapping_add(x))
}

/// The number of loads `stride_sum(v, stride)` makes.
pub fn accesses(len: usize, stride: usize) -> usize { len.div_ceil(stride) }

/// Evict, from every level, each line `stride_sum(v, stride)` touched, so the
/// next pass starts from DRAM. Without it a large stride's few thousand lines
/// (8,192 at 4,096 elements: 512 KB) stay in L2 or L3 between passes and the
/// plateau is a cache hit. `v` must start on a line, as a `Buffer` does.
pub fn flush_touched(v: &[u64], stride: usize) {
    let step = stride.max(LINE / 8); // below eight elements every line is touched
    if has_clflushopt() {
        for x in v.iter().step_by(step) {
            // SAFETY: the CPU has clflushopt (checked); it takes any mapped address, and `x` is live.
            unsafe { asm!("clflushopt [{}]", in(reg) x as *const u64, options(nostack, preserves_flags)) }
        }
    } else {
        for x in v.iter().step_by(step) {
            // SAFETY: clflush takes any mapped address; `x` is a live element of `v`.
            unsafe { _mm_clflush((x as *const u64).cast()) }
        }
    }
    // SAFETY: no operands; it orders the flushes before the next pass's loads.
    unsafe { _mm_mfence() }
}

/// CPUID leaf 7, EBX bit 23. `clflush` waits for each line in turn (about
/// 250 ns a line on the VM these runs came from, a second per 256 MB);
/// `clflushopt` overlaps them (about 3 ns a line). The flush is untimed
/// either way; this only shortens the run.
#[allow(unused_unsafe)] // the cpuid intrinsics are unsafe fns on older compilers, safe on newer ones
pub fn has_clflushopt() -> bool {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    // SAFETY: cpuid exists on every x86_64 part; leaf 0 gives the highest leaf there is.
    if unsafe { __cpuid(0) }.eax < 7 { return false; }
    // SAFETY: as above, and leaf 7 exists (checked).
    let leaf7 = unsafe { __cpuid_count(7, 0) };
    leaf7.ebx & (1 << 23) != 0
}

// ================================================================ Lab 1.4 pointer chasing

/// One node of Lab 1.4's list: a value and the link, in an allocation of its own.
pub struct Node {
    pub value: u64,
    pub next: Option<Box<Node>>,
}

/// A singly linked list of individually heap-allocated nodes.
pub struct List {
    pub head: Option<Box<Node>>,
    pub len: usize,
}

impl List {
    /// `order.len()` nodes, allocated one at a time with values 0, 1, 2, …, so
    /// the allocator lays them out in that order, then linked in the order
    /// `order` gives. Shuffled, each link lands on an unrelated address; in
    /// order (`--no-shuffle`), each link is the next allocation and the walk
    /// looks sequential to the prefetcher: the trap of section 1.4.
    pub fn build(order: &[u32]) -> List {
        let mut nodes: Vec<Option<Box<Node>>> =
            (0..order.len() as u64).map(|value| Some(Box::new(Node { value, next: None }))).collect();
        let mut head = None;
        for &k in order.iter().rev() {
            let mut node = nodes[k as usize].take().expect("order is a permutation");
            node.next = head;
            head = Some(node);
        }
        List { head, len: order.len() }
    }
}

impl Drop for List {
    // One node at a time: the default drop recurses once per node and overflows the stack.
    fn drop(&mut self) {
        let mut cur = self.head.take();
        while let Some(mut node) = cur {
            cur = node.next.take();
        }
    }
}

/// Lab 1.4, the list: the next address is inside the node just loaded, so
/// only one miss is ever in flight.
#[inline(never)]
pub fn list_sum(list: &List) -> u64 {
    let mut sum = 0u64;
    let mut cur = list.head.as_deref();
    while let Some(node) = cur {
        sum = sum.wrapping_add(node.value);
        cur = node.next.as_deref();
    }
    sum
}

/// Lab 1.4, the `Vec`: read in order, eight values a line, streamed by the prefetchers.
#[inline(never)]
pub fn vec_sum(v: &[u64]) -> u64 {
    v.iter().fold(0u64, |s, &x| s.wrapping_add(x))
}

/// Section 1.4's case in between, `Vec<Box<u64>>`: the boxes are scattered,
/// but the pointers sit side by side, so the loads they lead to are
/// independent and several misses overlap.
#[inline(never)]
pub fn boxes_sum(v: &[Box<u64>]) -> u64 {
    v.iter().fold(0u64, |s, x| s.wrapping_add(**x))
}

/// `order.len()` boxes allocated in value order, then placed in the `Vec` in `order`.
pub fn boxes(order: &[u32]) -> Vec<Box<u64>> {
    let mut b: Vec<Option<Box<u64>>> = (0..order.len() as u64).map(|v| Some(Box::new(v))).collect();
    order.iter().map(|&k| b[k as usize].take().expect("order is a permutation")).collect()
}

// ================================================================ Lab 1.3 two threads, two counters

/// `iters` relaxed increments of `c`, timed in blocks of `block`: one
/// (increments, ticks) pair per block, into room made before the start.
#[inline(never)]
pub fn hammer(c: &AtomicU64, iters: u64, block: u64, out: &mut Vec<(u64, u64)>) {
    let mut done = 0;
    while done < iters {
        let n = block.min(iters - done);
        let t0 = now();
        for _ in 0..n {
            c.fetch_add(1, Ordering::Relaxed);
        }
        let t1 = now();
        if out.len() < out.capacity() { out.push((n, t1 - t0)); } // never grows: no allocation in the loop
        done += n;
    }
}

/// What one of Lab 1.3's threads saw.
pub struct Hammered {
    pub pinned: Pinned,          // how this thread's pin went
    pub start: u64,              // TSC at its first increment
    pub end: u64,                // TSC after its last
    pub blocks: Vec<(u64, u64)>, // (increments, ticks) per block
}

/// Two threads, one per counter, each pinned as asked, released together by a
/// barrier so their increments overlap.
pub fn run_pair(a: &AtomicU64, b: &AtomicU64, cores: [CoreArg; 2], iters: u64, block: u64) -> [Hammered; 2] {
    let barrier = Barrier::new(2);
    let block = block.max(1);
    std::thread::scope(|s| {
        let ha = s.spawn(|| one_thread(a, cores[0], &barrier, iters, block));
        let hb = s.spawn(|| one_thread(b, cores[1], &barrier, iters, block));
        [ha.join().expect("thread a panicked"), hb.join().expect("thread b panicked")]
    })
}

fn one_thread(c: &AtomicU64, core: CoreArg, barrier: &Barrier, iters: u64, block: u64) -> Hammered {
    let pinned = pin_as(core);                                     // pin first, then make room
    let mut out = Vec::with_capacity(iters.div_ceil(block) as usize);
    barrier.wait();                                                // both pinned and ready
    let start = now();
    hammer(c, iters, block, &mut out);
    let end = now();
    Hammered { pinned, start, end, blocks: out }
}

// ================================================================ the clock

/// The TSC, read once every earlier instruction has finished.
#[inline(always)]
pub fn now() -> u64 {
    // SAFETY: lfence and rdtsc exist on every x86_64 part and touch no memory.
    unsafe {
        _mm_lfence(); // wait for earlier instructions: do not read early
        _rdtsc()
    }
}

/// TSC ticks per nanosecond, measured once at startup against
/// CLOCK_MONOTONIC_RAW, the kernel clock the time daemon never slews.
/// The TSC ticks at a fixed nominal rate, so its nanoseconds are real time,
/// but its ticks are not the cycles the core spent (section 1.2).
pub fn calibrate() -> f64 {
    let raw = || { // CLOCK_MONOTONIC_RAW, in ns
        let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        // SAFETY: clock_gettime only writes ts, a live local.
        unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts) };
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    };
    let (n0, t0) = (raw(), now());
    while raw() - n0 < 200_000_000 {} // spin for 200 ms
    let (n1, t1) = (raw(), now());
    (t1 - t0) as f64 / (n1 - n0) as f64
}

// ================================================================ pinning

/// What `--core` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CoreArg {
    /// No flag: the chapter's core, unless the process was started on a single
    /// CPU (`taskset -c 3 ./target/release/stride`), which is then kept.
    Default(usize),
    /// `--core N`: that core.
    Core(usize),
    /// `--core none`: unpinned, on purpose.
    None,
}

/// The CPUs the calling thread may run on.
pub fn allowed_cpus() -> Vec<usize> {
    // SAFETY: cpu_set_t is plain bits; all zeros is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: pid 0 is the calling thread; the size is the set's own.
    if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
        return Vec::new();
    }
    // SAFETY: every index is below CPU_SETSIZE, the set's capacity.
    (0..libc::CPU_SETSIZE as usize).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).collect()
}

/// Pin the calling thread to `core`, or say why not.
pub fn pin(core: usize) -> Result<(), String> {
    if core >= libc::CPU_SETSIZE as usize {
        return Err(format!("core {core} is beyond CPU_SETSIZE"));
    }
    // SAFETY: cpu_set_t is plain bits; all zeros is the empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: core < CPU_SETSIZE (checked above), so the bit is inside the set.
    unsafe { libc::CPU_SET(core, &mut set) };
    // SAFETY: pid 0 is the calling thread; the set is initialised and sized.
    if unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) } == 0 {
        Ok(())
    } else {
        Err(format!("core {core}: {}", std::io::Error::last_os_error()))
    }
}

/// How a pin went: the line the report prints, and whether the pin asked for was refused.
pub struct Pinned {
    pub line: String,
    pub refused: bool,
}

/// Pin the calling thread as asked. A refused pin is a warning, not a stop:
/// the line says so, and that the numbers are from an unpinned thread.
pub fn pin_as(arg: CoreArg) -> Pinned {
    let allowed = allowed_cpus();
    let ok = |line: String| Pinned { line, refused: false };
    let c = match arg {
        CoreArg::None => return ok("pinned: no (--core none)".to_string()),
        CoreArg::Default(_) if allowed.len() == 1 => {
            return ok(format!("pinned: core {} (the single CPU this process was started on)", allowed[0]));
        }
        CoreArg::Default(c) | CoreArg::Core(c) => c,
    };
    match pin(c) {
        Ok(()) => ok(format!("pinned: core {c}")),
        Err(e) => Pinned {
            line: format!("pinned: NO ({e}; this process may use CPUs {}); name one of those \
                           (--core N, or --cores A,B for two threads). The numbers below are from \
                           an unpinned thread", cpu_list(&allowed)),
            refused: true,
        },
    }
}

/// `pin_as` for a binary's main thread: the report line, and the warning on stderr too.
pub fn pin_line(arg: CoreArg) -> String {
    let p = pin_as(arg);
    if p.refused { eprintln!("warning: {}", p.line); }
    p.line
}

/// "0,1,2" for a list of CPUs.
pub fn cpu_list(v: &[usize]) -> String {
    v.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")
}

/// Parse sysfs's CPU-list format, "0-3,8,10-11".
pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) { out.extend(a..=b); }
            }
            None => out.extend(part.parse::<usize>().ok()),
        }
    }
    out
}

/// The hyperthread siblings of `core`, itself included, from sysfs.
pub fn thread_siblings(core: usize) -> Option<Vec<usize>> {
    let p = format!("/sys/devices/system/cpu/cpu{core}/topology/thread_siblings_list");
    std::fs::read_to_string(p).ok().map(|s| parse_cpu_list(&s))
}

/// The socket `core` sits on, from sysfs.
pub fn package(core: usize) -> Option<u32> {
    let p = format!("/sys/devices/system/cpu/cpu{core}/topology/physical_package_id");
    std::fs::read_to_string(p).ok()?.trim().parse().ok()
}

// ================================================================ percentiles

/// Count, p50, p99, p99.9 and max of a set of samples. Never a mean: it blends
/// the common case with the rare slow one, and the rare one is where the money goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Summary {
    pub n: usize,
    pub p50: f64,
    pub p99: f64,
    pub p999: f64,
    pub max: f64,
}

impl Summary {
    /// Nearest-rank percentiles; sorts `v` in place. `v` must not be empty.
    pub fn of(v: &mut [f64]) -> Summary {
        assert!(!v.is_empty(), "no samples");
        v.sort_unstable_by(f64::total_cmp);
        let rank = |q: f64| v[((q * v.len() as f64).ceil() as usize).clamp(1, v.len()) - 1];
        Summary { n: v.len(), p50: rank(0.50), p99: rank(0.99), p999: rank(0.999), max: v[v.len() - 1] }
    }
}

// ================================================================ the seeded generator

/// SplitMix64: an add and two multiply-xorshifts per value. The same seed gives
/// the same data, so two runs measure the same bytes.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng { Rng(seed) }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in 0..n, by multiply-and-shift: no division, no retry loop.
    pub fn below(&mut self, n: u64) -> u64 { ((self.next_u64() as u128 * n as u128) >> 64) as u64 }
}

/// `n` uniform random bytes, every page written once.
pub fn random_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let mut v = vec![0u8; n];
    let mut chunks = v.chunks_exact_mut(8);
    for c in &mut chunks { c.copy_from_slice(&rng.next_u64().to_le_bytes()); }
    let tail = chunks.into_remainder();
    let last = rng.next_u64().to_le_bytes();
    tail.copy_from_slice(&last[..tail.len()]);
    v
}

/// 0..n in random order (Fisher-Yates), or in order when `shuffle` is false.
pub fn permutation(n: usize, shuffle: bool, seed: u64) -> Vec<u32> {
    assert!(n <= u32::MAX as usize, "at most 2^32 nodes");
    let mut p: Vec<u32> = (0..n as u32).collect();
    if shuffle {
        let mut rng = Rng::new(seed);
        for i in (1..n).rev() {
            p.swap(i, rng.below(i as u64 + 1) as usize);
        }
    }
    p
}

// ================================================================ memory: alignment and huge pages

/// A 2 MB huge page: the alignment of every `Buffer`.
pub const HUGE_PAGE: usize = 2 << 20;

/// `len` values on a 2 MB boundary, every page written before anything is
/// timed. With `huge`, the kernel is first asked (`madvise(MADV_HUGEPAGE)`)
/// to back it with 2 MB pages, which needs no root while
/// /sys/kernel/mm/transparent_hugepage/enabled says `madvise` or `always`.
pub struct Buffer<T: Copy> {
    ptr: NonNull<T>,
    len: usize,
    layout: Layout,
}

impl<T: Copy> Buffer<T> {
    pub fn new(len: usize, value: T, huge: bool) -> Buffer<T> {
        let bytes = len.checked_mul(size_of::<T>()).expect("buffer too large");
        assert!(bytes > 0, "an empty buffer");
        // whole huge pages, so with `huge` no 4 KB tail is left over
        let layout = Layout::from_size_align(bytes.next_multiple_of(HUGE_PAGE), HUGE_PAGE).expect("buffer too large");
        // SAFETY: the layout has a non-zero size (checked above).
        let raw = unsafe { alloc(layout) }.cast::<T>();
        let ptr = NonNull::new(raw).unwrap_or_else(|| handle_alloc_error(layout));
        if huge {
            // SAFETY: a hint about our own allocation; it reads and writes no memory.
            let r = unsafe { libc::madvise(raw.cast(), layout.size(), libc::MADV_HUGEPAGE) };
            if r != 0 {
                eprintln!("warning: madvise(MADV_HUGEPAGE) refused: {}; 4 KB pages", std::io::Error::last_os_error());
            }
        }
        for i in 0..len {
            // SAFETY: i < len, inside the allocation; first touch here, not in a timed pass.
            unsafe { raw.add(i).write(value) }
        }
        Buffer { ptr, len, layout }
    }
}

impl<T: Copy> Deref for Buffer<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        // SAFETY: all len elements were written in new; the allocation lives as long as self.
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: Copy> DerefMut for Buffer<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as in deref, and &mut self makes this the only reference.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T: Copy> Drop for Buffer<T> {
    fn drop(&mut self) {
        // SAFETY: allocated in new with exactly this layout, freed once.
        unsafe { dealloc(self.ptr.as_ptr().cast(), self.layout) }
    }
}

/// The transparent-huge-page mode, the bracketed word in sysfs: always, madvise or never.
pub fn thp_mode() -> String {
    std::fs::read_to_string("/sys/kernel/mm/transparent_hugepage/enabled").ok()
        .and_then(|s| s.split('[').nth(1).and_then(|r| r.split(']').next()).map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// How much of this process's anonymous memory sits on huge pages, in KB.
pub fn anon_huge_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/smaps_rollup").ok()?;
    let line = s.lines().find(|l| l.starts_with("AnonHugePages:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

// ================================================================ the report

/// One line naming the machine: CPU model, online CPUs, TSC rate, THP mode.
pub fn machine_line(ticks_per_ns: f64) -> String {
    let model = std::fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name"))
                       .and_then(|l| l.split(':').nth(1)).map(|m| m.trim().to_string()))
        .unwrap_or_else(|| "unknown CPU".to_string());
    let online = std::fs::read_to_string("/sys/devices/system/cpu/online").ok()
        .map_or_else(|| "unknown".to_string(), |s| cpu_list(&parse_cpu_list(&s)));
    format!("machine: {model}; CPUs online {online}; TSC {ticks_per_ns:.3} ticks/ns (times are TSC \
             nanoseconds, not core cycles); transparent huge pages: {}", thp_mode())
}

/// The line every binary prints first: the chapter's prediction, before any number.
pub fn predict(text: &str) {
    println!("Predict first (the chapter's prediction): {text}");
    println!("Write your own number and its reason down now, then read on.");
}

/// 1234567 as "1,234,567".
pub fn commas(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(c);
    }
    out
}

// ================================================================ flags

/// The flag parser every binary shares: `--flag value`, `-h`/`--help` prints the help.
pub struct Cli {
    name: &'static str,
    help: String,
    args: std::vec::IntoIter<String>,
}

impl Cli {
    pub fn new(name: &'static str, help: String) -> Cli {
        Cli { name, help, args: std::env::args().skip(1).collect::<Vec<_>>().into_iter() }
    }

    /// The next flag; `--help` prints the help and exits 0.
    pub fn flag(&mut self) -> Option<String> {
        let f = self.args.next()?;
        if f == "-h" || f == "--help" {
            println!("{}", self.help);
            exit(0);
        }
        Some(f)
    }

    pub fn value(&mut self, flag: &str) -> String {
        match self.args.next() {
            Some(v) => v,
            None => self.fail(&format!("{flag} needs a value")),
        }
    }

    /// A positive whole number; `100_000_000` is accepted.
    pub fn count(&mut self, flag: &str) -> u64 {
        let v = self.value(flag);
        match v.replace('_', "").parse::<u64>() {
            Ok(n) if n > 0 => n,
            _ => self.fail(&format!("{flag}: a positive whole number, not {v:?}")),
        }
    }

    /// `--core N` or `--core none`.
    pub fn core(&mut self, flag: &str) -> CoreArg {
        let v = self.value(flag);
        self.parse_core(flag, &v)
    }

    pub fn parse_core(&self, flag: &str, v: &str) -> CoreArg {
        if v == "none" { return CoreArg::None; }
        match v.parse() {
            Ok(c) => CoreArg::Core(c),
            Err(_) => self.fail(&format!("{flag}: a core number or none, not {v:?}")),
        }
    }

    pub fn fail(&self, msg: &str) -> ! {
        eprintln!("{}: {msg}\n(--help lists the flags)", self.name);
        exit(2)
    }
}

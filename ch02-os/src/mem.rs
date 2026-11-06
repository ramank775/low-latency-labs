//! The startup asks of "The quiet core", as calls that report a refusal
//! instead of panicking: an anonymous mapping with or without `MAP_POPULATE`
//! and `MAP_HUGETLB`, `madvise(MADV_HUGEPAGE)`, `mlockall`, the
//! `/dev/cpu_dma_latency` hold, and the thread's timer slack.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use crate::procfs::{kb_field, read_trim, sep};

pub const PAGE: usize = 4096;
pub const HUGE: usize = 2 << 20;

/// What `map` should ask for.
#[derive(Clone, Copy, Debug, Default)]
pub struct MapFlags {
    pub populate: bool, // MAP_POPULATE: every page faulted in inside the mmap call
    pub hugetlb: bool,  // MAP_HUGETLB: 2 MB pages from the reserved pool
    pub align_2m: bool, // start on a 2 MB boundary, so THP can back the whole range
}

/// An anonymous private mapping, unmapped on drop.
pub struct Region {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is owned by this value alone; moving it to another thread moves the ownership.
unsafe impl Send for Region {}

impl Region {
    pub fn len(&self) -> usize { self.len }
    pub fn is_empty(&self) -> bool { self.len == 0 }
    pub fn as_ptr(&self) -> *mut u8 { self.ptr }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: ptr..ptr+len is one readable, writable mapping owned by self, zero-filled by the kernel.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for Region {
    fn drop(&mut self) {
        // SAFETY: exactly the range mmap returned (or its aligned middle), not unmapped before.
        unsafe { libc::munmap(self.ptr.cast(), self.len) };
    }
}

/// `mmap(len, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS ...)`.
/// A `MAP_HUGETLB` refusal says how big the pool is and how to grow it.
pub fn map(len: usize, f: MapFlags) -> Result<Region, String> {
    let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
    if f.populate { flags |= libc::MAP_POPULATE; }
    if f.hugetlb { flags |= libc::MAP_HUGETLB; }
    let len = if f.hugetlb { len.div_ceil(HUGE) * HUGE } else { len.div_ceil(PAGE) * PAGE };
    // over-map by 2 MB and trim, so the kept range starts on a huge-page boundary
    let ask = if f.align_2m && !f.hugetlb { len + HUGE } else { len };
    // SAFETY: a fresh anonymous mapping at an address of the kernel's choosing touches nothing of ours.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), ask, libc::PROT_READ | libc::PROT_WRITE, flags, -1, 0) };
    if p == libc::MAP_FAILED {
        let e = io::Error::last_os_error();
        if f.hugetlb {
            let total = kb_field("/proc/meminfo", "HugePages_Total").unwrap_or(0);
            let free = kb_field("/proc/meminfo", "HugePages_Free").unwrap_or(0);
            return Err(format!(
                "mmap(MAP_HUGETLB, {} MB) refused: {e}. The pool has {total} huge pages, {free} free, and \
                 this needs {}; reserve them first (root): echo {} > /proc/sys/vm/nr_hugepages",
                len >> 20, len / HUGE, len / HUGE));
        }
        if f.populate || locked_future() {
            return Err(format!("mmap({} MB) refused: {e}. Under MCL_FUTURE every new mapping is locked, \
                                and RLIMIT_MEMLOCK is {}; raise it with `ulimit -l unlimited` or shrink --size",
                               len >> 20, memlock_limit()));
        }
        return Err(format!("mmap({} MB) refused: {e}", len >> 20));
    }
    let p = p.cast::<u8>();
    if ask == len { return Ok(Region { ptr: p, len }); }
    let start = (p as usize).div_ceil(HUGE) * HUGE;
    let head = start - p as usize;
    let tail = ask - head - len;
    // SAFETY: both trims lie inside the mapping just made and outside the range kept.
    unsafe {
        if head > 0 { libc::munmap(p.cast(), head); }
        if tail > 0 { libc::munmap((start + len) as *mut libc::c_void, tail); }
    }
    Ok(Region { ptr: start as *mut u8, len })
}

/// `madvise(MADV_HUGEPAGE)` on the region. Accepted and ignored when THP is
/// `never`, so the result says what the kernel's mode is.
pub fn advise_huge(r: &Region) -> Result<String, String> {
    // SAFETY: the range is exactly the live mapping `r` owns.
    let rc = unsafe { libc::madvise(r.ptr.cast(), r.len, libc::MADV_HUGEPAGE) };
    let mode = read_trim("/sys/kernel/mm/transparent_hugepage/enabled").unwrap_or_else(|| "absent".into());
    if rc != 0 {
        return Err(format!("madvise(MADV_HUGEPAGE) refused: {} (transparent_hugepage/enabled: {mode})",
                           io::Error::last_os_error()));
    }
    if crate::procfs::bracketed(&mode) == Some("never") {
        return Ok(format!("madvise accepted, but transparent_hugepage/enabled is `{mode}`: it will be ignored"));
    }
    Ok(format!("madvise(MADV_HUGEPAGE) accepted (transparent_hugepage/enabled: {mode})"))
}

// set while MCL_FUTURE is in force, so a refused mmap can say why
static LOCKED_FUTURE: AtomicBool = AtomicBool::new(false);

fn locked_future() -> bool {
    LOCKED_FUTURE.load(Relaxed)
}

/// `mlockall(MCL_CURRENT | MCL_FUTURE)`: every page now and every later
/// mapping resident and filled in at creation. A refusal names the limit.
pub fn lock_all() -> Result<(), String> {
    // SAFETY: mlockall takes two flags and touches no memory of ours.
    if unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) } == 0 {
        LOCKED_FUTURE.store(true, Relaxed);
        return Ok(());
    }
    Err(format!("mlockall(MCL_CURRENT | MCL_FUTURE) refused: {}. RLIMIT_MEMLOCK is {}; it needs \
                 `ulimit -l unlimited` or CAP_IPC_LOCK", io::Error::last_os_error(), memlock_limit()))
}

pub fn unlock_all() {
    // SAFETY: munlockall takes no arguments and touches no memory of ours.
    unsafe { libc::munlockall() };
    LOCKED_FUTURE.store(false, Relaxed);
}

/// The soft locked-memory limit, as `ulimit -l` would put it.
pub fn memlock_limit() -> String {
    // SAFETY: an all-zero rlimit is valid, and getrlimit writes only into it.
    let mut r: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `r` is a live, correctly typed local.
    unsafe { libc::getrlimit(libc::RLIMIT_MEMLOCK, &mut r) };
    if r.rlim_cur == libc::RLIM_INFINITY { "unlimited".into() } else { format!("{} KB", sep(r.rlim_cur / 1024)) }
}

/// The power-management QoS hold: open `/dev/cpu_dma_latency`, write a 32-bit
/// zero and keep the file open. No core enters an idle state deeper than
/// polling until the returned file is dropped.
pub fn hold_c0() -> Result<File, String> {
    let mut f = OpenOptions::new().write(true).open("/dev/cpu_dma_latency")
        .map_err(|e| format!("cannot open /dev/cpu_dma_latency: {e} (it needs root, or write permission on the device)"))?;
    f.write_all(&0i32.to_ne_bytes()).map_err(|e| format!("cannot write /dev/cpu_dma_latency: {e}"))?;
    Ok(f)
}

/// `prctl(PR_SET_TIMERSLACK, ns)` for the calling thread; 0 restores the default.
pub fn set_timer_slack(ns: u64) -> Result<(), String> {
    // SAFETY: PR_SET_TIMERSLACK takes one integer and touches no memory of ours.
    let rc = unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, ns as libc::c_ulong, 0, 0, 0) };
    if rc == 0 { Ok(()) } else { Err(format!("prctl(PR_SET_TIMERSLACK, {ns}) refused: {}", io::Error::last_os_error())) }
}

/// The calling thread's timer slack in nanoseconds: 50,000 for a normal thread.
pub fn timer_slack() -> i64 {
    // SAFETY: PR_GET_TIMERSLACK returns the value and touches no memory of ours.
    unsafe { libc::prctl(libc::PR_GET_TIMERSLACK, 0, 0, 0, 0) as i64 }
}

/// The idle states a core offers, `(name, exit latency in µs)`; empty in most VMs.
pub fn cpuidle_states(core: usize) -> Vec<(String, u64)> {
    let mut v = Vec::new();
    for i in 0.. {
        let base = format!("/sys/devices/system/cpu/cpu{core}/cpuidle/state{i}");
        let Some(name) = read_trim(&format!("{base}/name")) else { break };
        let lat = read_trim(&format!("{base}/latency")).and_then(|s| s.parse().ok()).unwrap_or(0);
        v.push((name, lat));
    }
    v
}

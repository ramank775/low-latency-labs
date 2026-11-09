//! What the labs ask of the operating system and read back from it: pinning,
//! the kernel's protocol counters, a thread's context switches and CPU time,
//! the C-state hold. Everything that needs a privilege says so and carries on.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;

/// The cores this process may run on.
pub fn available_cores() -> Vec<usize> {
    // SAFETY: a zeroed cpu_set_t is a valid empty set; the call writes at most its size.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: as above; pid 0 is the calling thread.
    let rc = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 { return Vec::new(); }
    // SAFETY: CPU_ISSET reads the set filled above, for indices below CPU_SETSIZE.
    (0..libc::CPU_SETSIZE as usize).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).collect()
}

/// Pins the calling thread to `core`, or says why it could not.
pub fn pin(core: usize) -> Result<(), String> {
    // SAFETY: a zeroed cpu_set_t is a valid empty set.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if core >= libc::CPU_SETSIZE as usize { return Err(format!("core {core} is beyond CPU_SETSIZE")); }
    // SAFETY: core < CPU_SETSIZE, checked above.
    unsafe { libc::CPU_SET(core, &mut set) };
    // SAFETY: the set is initialised and its size is passed; pid 0 is this thread.
    let rc = unsafe { libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set) };
    if rc == 0 { return Ok(()); }
    let e = std::io::Error::last_os_error();
    Err(format!("cannot pin to core {core} ({e}); this process may use cores {:?}", available_cores()))
}

/// Pins, or prints the reason and runs unpinned: the numbers are then a
/// stock box's, and the output says so.
pub fn pin_or_say(core: Option<usize>, who: &str) -> bool {
    let Some(c) = core else { return false };
    match pin(c) {
        Ok(()) => true,
        Err(e) => { eprintln!("note: {who}: {e}: running unpinned"); false }
    }
}

/// `"A,B"` as two core numbers.
pub fn two_cores(s: &str) -> Option<(usize, usize)> {
    let (a, b) = s.split_once(',')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// The calling thread's voluntary and involuntary context switches.
pub fn ctxt_switches() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/thread-self/status").ok()?;
    let get = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.trim().parse().ok());
    Some((get("voluntary_ctxt_switches:")?, get("nonvoluntary_ctxt_switches:")?))
}

/// The calling thread's CPU time in nanoseconds.
pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime writes one timespec, a live local.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Box-wide protocol counters by nstat's names (`TcpExtDelayedACKs`,
/// `TcpOutSegs`, `UdpRcvbufErrors`...), read from /proc/net/netstat and
/// /proc/net/snmp, where each is a header line of names and a line of values.
pub fn counters(names: &[&str]) -> Vec<Option<u64>> {
    let mut all: Vec<(String, u64)> = Vec::new();
    for path in ["/proc/net/netstat", "/proc/net/snmp"] {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        for pair in lines.chunks(2) {
            let [head, vals] = pair else { continue };
            let (Some((proto, keys)), Some((_, nums))) = (head.split_once(':'), vals.split_once(':')) else { continue };
            for (k, v) in keys.split_whitespace().zip(nums.split_whitespace()) {
                if let Ok(v) = v.parse::<i64>() { all.push((format!("{proto}{k}"), v as u64)); }
            }
        }
    }
    names.iter().map(|n| all.iter().find(|(k, _)| k == n).map(|&(_, v)| v)).collect()
}

/// A sysctl's value, as text, from /proc/sys.
pub fn sysctl(name: &str) -> Option<String> {
    let path = format!("/proc/sys/{}", name.replace('.', "/"));
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// SO_BUSY_POLL at 50 µs, or a message saying what it needs and what the run
/// measures instead. True when set.
pub fn busy_poll_or_say(sock: &impl AsRawFd, who: &str) -> bool {
    match crate::set_busy_poll(sock) {
        Ok(()) => true,
        Err(e) => {
            let floor = sysctl("net.core.busy_read").unwrap_or_else(|| "?".into());
            eprintln!("note: {who}: SO_BUSY_POLL at 50 µs refused ({e}). Before kernel 6.4, above \
                       net.core.busy_read (here {floor}) it needs CAP_NET_ADMIN: run as root, or have root set \
                       `sysctl -w net.core.busy_read=50`. This run measures --spin.");
            false
        }
    }
}

/// Chapter 2's C-state hold: /dev/cpu_dma_latency opened and 0 written to it
/// keeps every core of the box out of deep C-states for as long as the file
/// stays open. Needs write access to the device, root on a stock box.
pub fn hold_c0() -> Result<File, String> {
    let mut f = std::fs::OpenOptions::new().write(true).open("/dev/cpu_dma_latency")
        .map_err(|e| format!("cannot open /dev/cpu_dma_latency for writing ({e}); it needs root"))?;
    f.write_all(&0i32.to_ne_bytes()).map_err(|e| format!("writing 0 to /dev/cpu_dma_latency: {e}"))?;
    Ok(f)
}

/// fork(2), for the labs whose far side is another process. Called before any
/// thread is spawned, so the child is a whole copy of a single-threaded program.
pub fn fork() -> std::io::Result<libc::pid_t> {
    // SAFETY: every caller forks while the process has one thread.
    let pid = unsafe { libc::fork() };
    if pid < 0 { Err(std::io::Error::last_os_error()) } else { Ok(pid) }
}

/// In a forked child: be sent SIGTERM when the parent dies, so a spinning echo
/// never outlives the run that started it.
pub fn die_with_parent() {
    // SAFETY: prctl with PR_SET_PDEATHSIG takes a signal number and touches no memory.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) };
}

/// Waits for a child forked above; true when it exited with status 0.
pub fn wait_child(pid: libc::pid_t) -> bool {
    let mut status = 0;
    // SAFETY: waitpid writes one c_int, a live local.
    let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
    rc == pid && libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0
}

/// Ends a forked child without running the parent's destructors or atexit handlers.
pub fn exit_child(code: i32) -> ! {
    let _ = std::io::stdout().flush();
    // SAFETY: _exit ends the process at once; nothing after it runs.
    unsafe { libc::_exit(code) }
}

/// The kernel and the TSC rate, for the head of every run's output.
pub fn describe_box(ticks_per_ns: f64) -> String {
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into());
    // SAFETY: sysconf reads a constant of the running system.
    let online = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    format!("kernel {kernel}, TSC {ticks_per_ns:.3} GHz, {online} cores online, this process may use {:?}", available_cores())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_the_kernel_counts() {
        assert!(!available_cores().is_empty());
        let (v, n) = ctxt_switches().expect("/proc/thread-self/status");
        assert!(v + n < u64::MAX);
        let c = counters(&["TcpOutSegs", "TcpExtDelayedACKs", "NoSuchCounter"]);
        assert!(c[0].is_some(), "TcpOutSegs from /proc/net/snmp");
        assert!(c[1].is_some(), "TcpExtDelayedACKs from /proc/net/netstat");
        assert!(c[2].is_none());
        assert!(sysctl("net.core.busy_read").is_some());
        let a = thread_cpu_ns();
        let mut x = 0u64;
        for i in 0..1_000_000u64 { x = x.wrapping_add(std::hint::black_box(i)); }
        assert!(thread_cpu_ns() > a && x > 0);
    }

    #[test]
    fn pinning_to_an_allowed_core_works_and_to_a_missing_one_says_so() {
        let first = available_cores()[0];
        assert!(pin(first).is_ok());
        assert!(pin(1023).unwrap_err().contains("cannot pin"));
        assert_eq!(two_cores("2, 3"), Some((2, 3)));
    }
}

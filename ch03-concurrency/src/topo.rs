//! Pinning, and what two cores share. The lab's rules ask every run to record
//! the architecture and, for the cores it used, whether they are hyperthread
//! siblings, share one L3, sit on two L3s of one socket, or on two sockets:
//! the four rows of the ping-pong table. This reads the same facts `lscpu -e`
//! prints, from `/sys/devices/system/cpu`.

use std::fs;

/// The CPUs this process may run on (its affinity mask, after any cpuset).
#[cfg(target_os = "linux")]
pub fn available_cores() -> Vec<usize> {
    // SAFETY: cpu_set_t is plain bits; zeroed is the empty set, and the kernel fills it.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: pid 0 is this thread, and the size passed is the size of `set`.
    let rc = unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set) };
    if rc != 0 {
        return (0..std::thread::available_parallelism().map_or(1, |n| n.get())).collect();
    }
    // SAFETY: CPU_ISSET only reads the set, for indices below CPU_SETSIZE.
    (0..libc::CPU_SETSIZE as usize).filter(|&c| unsafe { libc::CPU_ISSET(c, &set) }).collect()
}

#[cfg(not(target_os = "linux"))]
pub fn available_cores() -> Vec<usize> {
    (0..std::thread::available_parallelism().map_or(1, |n| n.get())).collect()
}

/// Pin the calling thread to `core`.
#[cfg(target_os = "linux")]
pub fn pin(core: usize) -> Result<(), String> {
    if core >= libc::CPU_SETSIZE as usize {
        return Err(format!("core {core} is beyond CPU_SETSIZE"));
    }
    // SAFETY: as in available_cores: an empty set, then one bit set below CPU_SETSIZE.
    let rc = unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_SET(core, &mut set);
        libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &set)
    };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error().to_string()) }
}

#[cfg(not(target_os = "linux"))]
pub fn pin(core: usize) -> Result<(), String> {
    Err(format!("pinning to core {core} is implemented for Linux only"))
}

/// Pin to `core`, or say clearly why not and carry on unpinned. Returns
/// whether the thread is pinned. The chapter does not ask a refused pin to
/// stop the lab, so it does not; but an unpinned number is not the lab's number.
pub fn pin_or_say(core: Option<usize>, who: &str) -> bool {
    let Some(core) = core else { return false };
    let allowed = available_cores();
    if !allowed.contains(&core) {
        eprintln!("pin: {who}: core {core} is not available to this process (allowed: {}); \
                   {who} runs unpinned. Pass cores from that list.", list(&allowed));
        return false;
    }
    match pin(core) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("pin: {who}: sched_setaffinity to core {core} failed ({e}); {who} runs unpinned.");
            false
        }
    }
}

/// The CPU the calling thread is on right now: the check that a pin took.
#[cfg(target_os = "linux")]
pub fn current_cpu() -> Option<usize> {
    // SAFETY: sched_getcpu takes no arguments and only returns a number.
    let c = unsafe { libc::sched_getcpu() };
    (c >= 0).then_some(c as usize)
}

#[cfg(not(target_os = "linux"))]
pub fn current_cpu() -> Option<usize> { None }

/// This thread's (voluntary, involuntary) context switches so far, from
/// getrusage(RUSAGE_THREAD): the count `perf stat -e context-switches` gives,
/// without perf. A voluntary one is a thread that gave up its core, such as a
/// mutex loser asleep in futex; an involuntary one is a preemption.
#[cfg(target_os = "linux")]
pub fn thread_ctx_switches() -> (u64, u64) {
    // SAFETY: rusage is plain integers; zeroed is valid, and the kernel fills it for this thread.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: RUSAGE_THREAD names the calling thread, and `ru` is a live rusage.
    if unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) } != 0 { return (0, 0); }
    (ru.ru_nvcsw as u64, ru.ru_nivcsw as u64)
}

#[cfg(not(target_os = "linux"))]
pub fn thread_ctx_switches() -> (u64, u64) { (0, 0) }

/// What two logical CPUs share: the rows of Lab 3.3's table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Share {
    Same,
    Siblings,
    OneL3,
    TwoL3s,
    TwoSockets,
    Unknown,
}

impl Share {
    pub fn describe(self) -> &'static str {
        match self {
            Share::Same => "the same logical CPU",
            Share::Siblings => "hyperthread siblings: one core, one L1 and L2",
            Share::OneL3 => "two physical cores sharing one L3",
            Share::TwoL3s => "two L3s on one socket",
            Share::TwoSockets => "two sockets",
            Share::Unknown => "unknown (no topology in /sys)",
        }
    }
}

fn sys(cpu: usize, rest: &str) -> Option<String> {
    fs::read_to_string(format!("/sys/devices/system/cpu/cpu{cpu}/{rest}")).ok().map(|s| s.trim().to_string())
}

/// "0-3,8-11" to [0, 1, 2, 3, 8, 9, 10, 11].
pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut v = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) { v.extend(a..=b) }
            }
            None => {
                if let Ok(a) = part.parse() { v.push(a) }
            }
        }
    }
    v
}

/// [0, 1, 2, 3, 8] to "0-3,8".
pub fn list(cpus: &[usize]) -> String {
    let mut out = Vec::new();
    let mut i = 0;
    while i < cpus.len() {
        let mut j = i;
        while j + 1 < cpus.len() && cpus[j + 1] == cpus[j] + 1 { j += 1; }
        out.push(if j > i { format!("{}-{}", cpus[i], cpus[j]) } else { cpus[i].to_string() });
        i = j + 1;
    }
    out.join(",")
}

/// The CPUs that share `cpu`'s L3: the cache index whose level is 3.
fn l3_of(cpu: usize) -> Option<Vec<usize>> {
    (0..8).find_map(|i| {
        (sys(cpu, &format!("cache/index{i}/level"))? == "3")
            .then(|| sys(cpu, &format!("cache/index{i}/shared_cpu_list")).map(|s| parse_cpu_list(&s)))
            .flatten()
    })
}

pub fn share(a: usize, b: usize) -> Share {
    if a == b { return Share::Same; }
    let Some(sib) = sys(a, "topology/thread_siblings_list") else { return Share::Unknown };
    if parse_cpu_list(&sib).contains(&b) { return Share::Siblings; }
    if let Some(l3) = l3_of(a) {
        if l3.contains(&b) { return Share::OneL3; }
    }
    match (sys(a, "topology/physical_package_id"), sys(b, "topology/physical_package_id")) {
        (Some(x), Some(y)) if x == y => Share::TwoL3s,
        (Some(_), Some(_)) => Share::TwoSockets,
        _ => Share::Unknown,
    }
}

/// The lab's first line: architecture, CPUs, and the model name where /proc has it.
pub fn machine() -> String {
    let model = fs::read_to_string("/proc/cpuinfo").ok()
        .and_then(|s| s.lines().find(|l| l.starts_with("model name"))
                       .and_then(|l| l.split_once(':')).map(|(_, m)| m.trim().to_string()));
    let cores = available_cores();
    format!("machine: {} (uname -m), {} CPUs available to this process ({}){}",
            std::env::consts::ARCH, cores.len(), list(&cores),
            model.map_or(String::new(), |m| format!(", {m}")))
}

/// "core 2 and core 3: two physical cores sharing one L3", or the unpinned case.
pub fn placement(a: Option<usize>, b: Option<usize>) -> String {
    let here = available_cores();
    match (a, b) {
        (Some(a), Some(b)) if !here.contains(&a) || !here.contains(&b) =>
            format!("cores {a} and {b}: not both available to this process (allowed: {}), so the threads run unpinned",
                    list(&here)),
        (Some(a), Some(b)) => format!("cores {a} and {b}: {}", share(a, b).describe()),
        _ => "unpinned: the scheduler chooses, and may move the threads".to_string(),
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn cpu_lists_round_trip() {
        assert_eq!(parse_cpu_list("0-3,8-11"), vec![0, 1, 2, 3, 8, 9, 10, 11]);
        assert_eq!(parse_cpu_list("5"), vec![5]);
        assert_eq!(list(&[0, 1, 2, 3, 8]), "0-3,8");
        assert_eq!(list(&parse_cpu_list("0-1,4,6-7")), "0-1,4,6-7");
    }

    #[test]
    fn a_core_shares_everything_with_itself() {
        assert_eq!(share(0, 0), Share::Same);
        assert!(!available_cores().is_empty());
    }
}

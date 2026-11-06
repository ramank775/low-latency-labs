//! What the kernel says about a run, read from /proc and /sys before and
//! after it: the interrupt and softirq counters per core, the thread's own
//! context switches and migrations, the host's steal time, the fault counts.
//! Every read allocates, so none of them belongs inside a measured loop.

use std::fs;

/// `/proc/interrupts` or `/proc/softirqs`: one row per source, one column per core.
#[derive(Clone, Debug, Default)]
pub struct Table {
    pub cpus: usize,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug)]
pub struct Row {
    pub name: String,      // "LOC", "RES", "37", "TIMER"
    pub per_cpu: Vec<u64>, // shorter than `cpus` for ERR and MIS, which are box-wide
    pub desc: String,      // "Local timer interrupts", "virtio1-req.0"; empty in softirqs
}

impl Table {
    /// The header names the cores (`CPU0 CPU1 ...`); each row is `name:` and the counts.
    pub fn parse(text: &str) -> Table {
        let mut lines = text.lines();
        let cpus = lines.next().map_or(0, |h| h.split_whitespace().filter(|w| w.starts_with("CPU")).count());
        let mut rows = Vec::new();
        for line in lines {
            let Some((name, rest)) = line.split_once(':') else { continue };
            let mut per_cpu = Vec::with_capacity(cpus);
            let mut words = rest.split_whitespace().peekable();
            while per_cpu.len() < cpus {
                match words.peek().and_then(|w| w.parse::<u64>().ok()) {
                    Some(v) => { per_cpu.push(v); words.next(); }
                    None => break,
                }
            }
            // a device row's description ends with the driver's name for the line
            let desc = words.collect::<Vec<_>>().join(" ");
            rows.push(Row { name: name.trim().to_string(), per_cpu, desc });
        }
        Table { cpus, rows }
    }

    pub fn read(path: &str) -> Table {
        Table::parse(&fs::read_to_string(path).unwrap_or_default())
    }

    /// The rows that moved on `core` between `self` (before) and `after`, largest first.
    pub fn deltas(&self, after: &Table, core: usize) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        for a in &after.rows {
            let Some(b) = self.rows.iter().find(|r| r.name == a.name) else { continue };
            let (Some(&x), Some(&y)) = (b.per_cpu.get(core), a.per_cpu.get(core)) else { continue };
            if y > x {
                let label = if a.name.chars().all(|c| c.is_ascii_digit()) {
                    // a device line: its number means nothing without the driver's name
                    format!("{} {}", a.name, a.desc.rsplit(' ').next().unwrap_or(""))
                } else {
                    a.name.clone()
                };
                out.push((label, y - x));
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.1));
        out
    }

    /// One row's count on one core, for the `LOC` delta the lab asks for.
    pub fn get(&self, name: &str, core: usize) -> Option<u64> {
        self.rows.iter().find(|r| r.name == name).and_then(|r| r.per_cpu.get(core).copied())
    }
}

/// The thread's own counters: the cheapest jitter test there is.
#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadCounters {
    pub voluntary: u64,     // gave the core up by blocking
    pub nonvoluntary: u64,  // preempted while it still wanted the core
    pub migrations: Option<u64>, // moved by the balancer; None when the kernel has no sched file
}

impl ThreadCounters {
    /// The calling thread's counters, from /proc/thread-self.
    pub fn read() -> ThreadCounters {
        let status = fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
        let field = |name: &str| {
            status.lines().find_map(|l| l.strip_prefix(name)?.trim_start_matches(':').trim().parse().ok())
        };
        let sched = fs::read_to_string("/proc/thread-self/sched").unwrap_or_default();
        let migrations = sched.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.trim().ends_with("nr_migrations") { v.trim().parse().ok() } else { None }
        });
        ThreadCounters {
            voluntary: field("voluntary_ctxt_switches").unwrap_or(0),
            nonvoluntary: field("nonvoluntary_ctxt_switches").unwrap_or(0),
            migrations,
        }
    }

    pub fn line(&self, after: &ThreadCounters) -> String {
        let mig = match (self.migrations, after.migrations) {
            (Some(b), Some(a)) => format!("+{}", a.saturating_sub(b)),
            _ => "unknown (no /proc/thread-self/sched)".to_string(),
        };
        format!("voluntary_ctxt_switches +{}, nonvoluntary_ctxt_switches +{}, nr_migrations {mig}",
                after.voluntary.saturating_sub(self.voluntary),
                after.nonvoluntary.saturating_sub(self.nonvoluntary))
    }
}

/// The `steal` column of `/proc/stat`, per core, in clock ticks (USER_HZ):
/// time the host gave to something other than this guest.
pub fn steal_ticks() -> Vec<u64> {
    let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
    let mut v = Vec::new();
    for l in stat.lines() {
        let mut w = l.split_whitespace();
        let Some(name) = w.next() else { continue };
        if name.len() > 3 && name.starts_with("cpu") {
            v.push(w.nth(7).and_then(|s| s.parse().ok()).unwrap_or(0)); // user nice system idle iowait irq softirq steal
        }
    }
    v
}

/// Everything the lab compares before and after a run.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub interrupts: Table,
    pub softirqs: Table,
    pub thread: ThreadCounters,
    pub steal: Vec<u64>,
}

impl Snapshot {
    /// Taken by the thread whose counters it should hold.
    pub fn take() -> Snapshot {
        Snapshot {
            interrupts: Table::read("/proc/interrupts"),
            softirqs: Table::read("/proc/softirqs"),
            thread: ThreadCounters::read(),
            steal: steal_ticks(),
        }
    }

    /// The deltas, core by core: half the result, as the chapter's rules say.
    pub fn report(&self, after: &Snapshot, cores: &[usize], secs: f64) {
        let ms_per_tick = 1000.0 / clk_tck() as f64;
        for &c in cores {
            let irq = self.interrupts.deltas(&after.interrupts, c);
            let loc = irq.iter().find(|(n, _)| n == "LOC").map_or(0, |r| r.1);
            println!("  /proc/interrupts core {c}: {}", join(&irq));
            println!("  LOC delta core {c}: {} ({:.0}/s: HZ on a busy core that never sleeps; each timed wake-up adds one)",
                     sep(loc), loc as f64 / secs.max(1e-9));
            println!("  /proc/softirqs   core {c}: {}", join(&self.softirqs.deltas(&after.softirqs, c)));
            if let (Some(b), Some(a)) = (self.steal.get(c), after.steal.get(c)) {
                println!("  steal core {c}: +{:.0} ms (the host's share of this guest's time)",
                         a.saturating_sub(*b) as f64 * ms_per_tick);
            }
        }
        println!("  this thread: {}", self.thread.line(&after.thread));
    }
}

fn join(rows: &[(String, u64)]) -> String {
    if rows.is_empty() { return "nothing moved".to_string(); }
    rows.iter().map(|(n, d)| format!("{n} +{}", sep(*d))).collect::<Vec<_>>().join(", ")
}

pub fn clk_tck() -> i64 {
    // SAFETY: sysconf reads a constant and touches no memory of ours.
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t } else { 100 }
}

/// The calling thread's minor and major fault counts. These count the faults
/// `MAP_POPULATE` and `mlockall` take inside the kernel too, which `perf stat
/// -e page-faults` leaves out: perf counts only faults a user instruction raised.
pub fn faults() -> (u64, u64) {
    // SAFETY: an all-zero rusage is a valid value, and getrusage writes only into it.
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    // SAFETY: `ru` is a live, correctly typed local.
    unsafe { libc::getrusage(libc::RUSAGE_THREAD, &mut ru) };
    (ru.ru_minflt as u64, ru.ru_majflt as u64)
}

/// A `Field:   1234 kB` line of `/proc/self/smaps_rollup` or `/proc/meminfo`.
pub fn kb_field(path: &str, field: &str) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    text.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim() == field { v.split_whitespace().next()?.parse().ok() } else { None }
    })
}

/// A sysfs or procfs file, trimmed; `None` when it does not exist here.
pub fn read_trim(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// The bracketed choice in a THP mode file: `always [madvise] never` is `madvise`.
pub fn bracketed(s: &str) -> Option<&str> {
    s.split_whitespace().find(|w| w.starts_with('['))?.strip_prefix('[')?.strip_suffix(']')
}

/// 1234567 as "1,234,567".
pub fn sep(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) { out.push(','); }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const INTERRUPTS: &str = "           CPU0       CPU1
 37:          0       6785 PCI-MSIX-0000:00:02.0   1-edge      virtio1-req.0
LOC:      59082      57800   Local timer interrupts
RES:       2091       1962   Rescheduling interrupts
ERR:          0
";

    #[test]
    fn parses_interrupts_and_names_device_lines() {
        let before = Table::parse(INTERRUPTS);
        assert_eq!(before.cpus, 2);
        assert_eq!(before.get("LOC", 1), Some(57800));
        assert_eq!(before.rows[0].desc.rsplit(' ').next(), Some("virtio1-req.0"));
        let after = Table::parse(&INTERRUPTS.replace("57800", "58050").replace("6785", "6790"));
        let d = before.deltas(&after, 1);
        assert_eq!(d, vec![("LOC".to_string(), 250), ("37 virtio1-req.0".to_string(), 5)]);
        assert!(before.deltas(&after, 0).is_empty());
    }

    #[test]
    fn parses_softirqs() {
        let t = Table::parse("                    CPU0       CPU1       \n          HI:          0          0\n       TIMER:       7612      10640\n");
        assert_eq!(t.get("TIMER", 1), Some(10640));
    }

    #[test]
    fn small_helpers() {
        assert_eq!(sep(1_234_567), "1,234,567");
        assert_eq!(sep(999), "999");
        assert_eq!(bracketed("always [madvise] never"), Some("madvise"));
        let t = ThreadCounters::read();
        assert!(t.voluntary + t.nonvoluntary < u64::MAX);
    }
}

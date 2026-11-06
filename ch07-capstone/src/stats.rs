//! The kernel's own counts for one thread, read from /proc: page faults,
//! context switches, migrations and the CPU it last ran on. These are the
//! counters that must not move on a hot thread after warm-up.
//!
//! Everything here reads into stack buffers through `open` and `read`, so a
//! thread can take its own counts at the end of the run, before the counting
//! allocator is read, without being counted.

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct ThreadStats {
    pub minflt: u64,              // page faults served without I/O
    pub majflt: u64,              // page faults that waited for the disk
    pub voluntary: u64,           // the thread blocked: a futex, a sleep, a read
    pub involuntary: u64,         // the scheduler took the core for something else
    pub migrations: Option<u64>,  // moves between cores; None without CONFIG_SCHED_DEBUG
    pub cpu: u32,                 // the CPU it last ran on
}

impl ThreadStats {
    /// Counts since `before`, the warm-up's end; `cpu` is the later one's.
    pub fn since(&self, before: &ThreadStats) -> ThreadStats {
        ThreadStats {
            minflt: self.minflt - before.minflt,
            majflt: self.majflt - before.majflt,
            voluntary: self.voluntary - before.voluntary,
            involuntary: self.involuntary - before.involuntary,
            migrations: self.migrations.zip(before.migrations).map(|(a, b)| a - b),
            cpu: self.cpu,
        }
    }

    /// Another thread of this process, by its kernel thread id.
    pub fn of(tid: i32) -> Option<ThreadStats> {
        let mut dir = [0u8; 48];
        let n = put(&mut dir, 0, b"/proc/self/task/");
        let n = put_num(&mut dir, n, tid as u64);
        read_all(&dir[..n])
    }

    /// The calling thread.
    pub fn own() -> Option<ThreadStats> {
        read_all(b"/proc/thread-self")
    }
}

/// The calling thread's kernel id, the one /proc and perf name it by.
pub fn gettid() -> i32 {
    // SAFETY: gettid takes no arguments and touches no memory.
    unsafe { libc::syscall(libc::SYS_gettid) as i32 }
}

fn read_all(dir: &[u8]) -> Option<ThreadStats> {
    let mut buf = [0u8; 8192];
    let stat = read(dir, b"/stat", &mut buf)?;
    // after the command name in parentheses, which may hold spaces: field 3 onward
    let close = stat.iter().rposition(|&c| c == b')')?;
    let mut fields = stat[close + 2..].split(|&c| c == b' ');
    let minflt = num(fields.nth(7)?)?;                 // field 10
    let majflt = num(fields.nth(1)?)?;                 // field 12
    let cpu = num(fields.nth(26)?)? as u32;            // field 39
    let status = read(dir, b"/status", &mut buf)?;
    let voluntary = after(status, b"\nvoluntary_ctxt_switches:")?;
    let involuntary = after(status, b"\nnonvoluntary_ctxt_switches:")?;
    let migrations = read(dir, b"/sched", &mut buf).and_then(|s| after(s, b"\nse.nr_migrations"));
    Some(ThreadStats { minflt, majflt, voluntary, involuntary, migrations, cpu })
}

/// `dir` + `file` read whole into `buf`, with no allocation.
fn read<'b>(dir: &[u8], file: &[u8], buf: &'b mut [u8]) -> Option<&'b [u8]> {
    let mut path = [0u8; 80];
    let n = put(&mut path, 0, dir);
    let n = put(&mut path, n, file);
    path[n] = 0;                                       // a C string
    // SAFETY: `path` is a NUL-terminated buffer that outlives the call.
    let fd = unsafe { libc::open(path.as_ptr().cast(), libc::O_RDONLY | libc::O_CLOEXEC) };
    if fd < 0 { return None; }
    let mut len = 0;
    while len < buf.len() {
        // SAFETY: writes at most buf.len() - len bytes into buf[len..], which we own.
        let r = unsafe { libc::read(fd, buf[len..].as_mut_ptr().cast(), buf.len() - len) };
        if r <= 0 { break; }
        len += r as usize;
    }
    // SAFETY: fd was opened above and is closed once.
    unsafe { libc::close(fd) };
    Some(&buf[..len])
}

fn put(dst: &mut [u8], at: usize, src: &[u8]) -> usize {
    dst[at..at + src.len()].copy_from_slice(src);
    at + src.len()
}

fn put_num(dst: &mut [u8], at: usize, mut v: u64) -> usize {
    let mut digits = [0u8; 20];
    let mut i = digits.len();
    loop {
        i -= 1;
        digits[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 { break; }
    }
    put(dst, at, &digits[i..])
}

fn num(s: &[u8]) -> Option<u64> {
    let digits = s.iter().take_while(|c| c.is_ascii_digit());
    let mut v = 0u64;
    let mut any = false;
    for &c in digits { v = v * 10 + (c - b'0') as u64; any = true; }
    any.then_some(v)
}

/// The number after `key`, past any spaces, tabs and a colon.
fn after(hay: &[u8], key: &[u8]) -> Option<u64> {
    let at = hay.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &hay[at..];
    let skip = rest.iter().take_while(|&&c| c == b' ' || c == b'\t' || c == b':').count();
    num(&rest[skip..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_its_own_counters_and_another_threads() {
        let own = ThreadStats::own().expect("/proc/thread-self");
        let by_tid = ThreadStats::of(gettid()).expect("/proc/self/task/<tid>");
        assert!(by_tid.minflt >= own.minflt);
        assert!(by_tid.voluntary >= own.voluntary);
    }

    #[test]
    fn small_parsers() {
        let mut b = [0u8; 32];
        let n = put_num(&mut b, 0, 1_204_117);
        assert_eq!(&b[..n], b"1204117");
        assert_eq!(after(b"x\nfoo:\t 42\n", b"\nfoo:"), Some(42));
        assert_eq!(num(b"7x"), Some(7));
        assert_eq!(num(b"x"), None);
    }
}

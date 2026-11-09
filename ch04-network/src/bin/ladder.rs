//! ladder: Lab 4.1, the latency ladder. The client sends a one-byte message
//! and waits for a one-byte reply, a million times after a warm-up of 100,000,
//! timing each round trip with rdtsc on its own side only.
//!
//!     ladder --threads | --shm | --unix | --tcp-loopback  [--spin | --busy-poll]
//!     ladder --tcp <host>[:port]  [--spin | --busy-poll]     the client, on the first machine
//!     ladder --echo               [--spin | --busy-poll]     the echo side, on the second
//!     options: --iters N --warmup N --cores A,B --core N --port N
//!
//! In the local modes the binary starts the echo side itself, on the second
//! core of `--cores` (default 2,3: two cores that share an L3). `--connect
//! host:port` and `--listen [addr:]port` are other names for `--tcp` and `--echo`.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::exit;
use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Release}};

use ch04_network::sys::{self, busy_poll_or_say, counters, ctxt_switches, pin_or_say};
use ch04_network::timing::{self, calibrate, commas, new_hist, now, ns};
use hdrhistogram::Histogram;

const HELP: &str = "\
ladder: Lab 4.1, the latency ladder: a one-byte message and a one-byte reply, timed with rdtsc
on the client only, p50 / p99 / max over --iters round trips after --warmup.

usage:
  ladder --threads              two pinned threads and one padded AtomicU64 (Chapter 3, for scale)
  ladder --shm                  two processes spinning on a padded word in a memfd page, mmap'd before fork
  ladder --unix                 two processes, a connected pair of Unix domain sockets, blocking reads
  ladder --tcp-loopback         two processes, a TCP connection to 127.0.0.1, blocking reads
  ladder --tcp <host>[:port]    the client, against `ladder --echo` on the second machine
  ladder --echo                 the echo side for --tcp: listens on 0.0.0.0:--port, serves one client at a time
     --connect host:port is --tcp host:port; --listen [addr:]port is --echo on that address

  --spin         sockets non-blocking, each side loops on recv on its pinned core (give it at both ends)
  --busy-poll    the same with SO_BUSY_POLL at 50 us on both sockets; a kernel before 6.4 that refuses it above
                 net.core.busy_read without CAP_NET_ADMIN gets a message, and the run measures --spin

options:
  --iters N      round trips timed (default 1,000,000)
  --warmup N     round trips before timing (default 100,000)
  --cores A,B    client core and echo core in the local modes (default 2,3; `none` for unpinned)
  --core N       the client's core with --tcp, the echo's with --echo (default 2 and 3)
  --port N       the TCP port for --tcp and --echo (default 9000)

Every TCP mode sets TCP_NODELAY, so the ladder measures the path and not Nagle.
Two machines, both busy-polling: `ladder --echo --busy-poll` there, then
`ladder --tcp <host> --busy-poll` here. The bypass rung, on cards Onload supports:
`onload --profile=latency ./ladder --echo` there and `onload --profile=latency ./ladder --tcp <host>` here.

Count each variant's system calls in a run of its own, never in a timed one:
  strace -c -f ./target/release/ladder --tcp-loopback --iters 100000
  perf stat -e syscalls:sys_enter_recvfrom ./target/release/ladder --tcp-loopback --spin   (the empty polls)
and check that busy polling happens at all: `nstat -az TcpExtBusyPollRxPackets` should rise by
about one a round trip at each end; this binary prints its change.";

#[derive(Clone, PartialEq)]
enum Mode { Threads, Shm, Unix, TcpLoopback, Tcp(String), Echo(String) }

#[derive(Clone, Copy, PartialEq)]
enum Wait { Blocking, Spin, BusyPoll }

struct Args {
    mode: Mode,
    wait: Wait,
    iters: u64,
    warmup: u64,
    client_core: Option<usize>,
    echo_core: Option<usize>,
    port: u16,
}

fn usage(msg: &str) -> ! {
    eprintln!("ladder: {msg}\n\n{HELP}");
    exit(2)
}

fn parse_args() -> Args {
    let mut mode = None;
    let mut a = Args { mode: Mode::Threads, wait: Wait::Blocking, iters: 1_000_000, warmup: 100_000,
                       client_core: Some(2), echo_core: Some(3), port: 9000 };
    let mut core = None;
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        let set = |m: &mut Option<Mode>, v: Mode| {
            if m.is_some() { usage("give one mode: --threads, --shm, --unix, --tcp-loopback, --tcp or --echo"); }
            *m = Some(v);
        };
        match flag.as_str() {
            "--threads" => set(&mut mode, Mode::Threads),
            "--shm" => set(&mut mode, Mode::Shm),
            "--unix" => set(&mut mode, Mode::Unix),
            "--tcp-loopback" => set(&mut mode, Mode::TcpLoopback),
            "--tcp" | "--connect" => { let h = val(); set(&mut mode, Mode::Tcp(h)) }
            "--echo" => set(&mut mode, Mode::Echo(String::new())),
            "--listen" => { let l = val(); set(&mut mode, Mode::Echo(l)) }
            "--spin" => a.wait = Wait::Spin,
            "--busy-poll" => a.wait = Wait::BusyPoll,
            "--iters" => a.iters = timing::count(&val()).unwrap_or_else(|| usage("--iters: a count")),
            "--warmup" => a.warmup = timing::count(&val()).unwrap_or_else(|| usage("--warmup: a count")),
            "--cores" => {
                let v = val();
                if v == "none" { (a.client_core, a.echo_core) = (None, None); }
                else {
                    let (c, e) = sys::two_cores(&v).unwrap_or_else(|| usage("--cores: two core numbers, A,B, or none"));
                    (a.client_core, a.echo_core) = (Some(c), Some(e));
                }
            }
            "--core" => {
                let v = val();
                core = Some(if v == "none" { None } else { Some(v.parse().unwrap_or_else(|_| usage("--core: a core number"))) });
            }
            "--port" => a.port = val().parse().unwrap_or_else(|_| usage("--port: a port number")),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    a.mode = mode.unwrap_or_else(|| usage("give a mode"));
    if a.iters == 0 { usage("--iters must be at least 1"); }
    if let Some(c) = core {
        match a.mode { Mode::Echo(_) => a.echo_core = c, _ => a.client_core = c }
    }
    a
}

fn label(a: &Args) -> String {
    let m = match &a.mode {
        Mode::Threads => "--threads".to_string(), Mode::Shm => "--shm".into(), Mode::Unix => "--unix".into(),
        Mode::TcpLoopback => "--tcp-loopback".into(), Mode::Tcp(h) => format!("--tcp {h}"),
        Mode::Echo(_) => "--echo".into(),
    };
    match a.wait { Wait::Blocking => m, Wait::Spin => format!("{m} --spin"), Wait::BusyPoll => format!("{m} --busy-poll") }
}

fn prediction(a: &Args) -> Vec<&'static str> {
    let mut p = match (&a.mode, a.wait) {
        (Mode::Threads, _) => vec!["--threads: one cache line each way at 50-80 ns a hop: 100-160 ns a round trip, 400-640 cycles at 4 GHz."],
        (Mode::Shm, _) => vec!["--shm: the same line, and the kernel, having built the mapping, stays out: 100-200 ns, --threads within noise."],
        (Mode::Unix, Wait::Blocking) => vec!["--unix: a system call and a copy each way and a wake-up at each end, no protocol: 5-15 us,",
                                             "20,000-60,000 cycles at 4 GHz, 25-150 times --shm."],
        (Mode::TcpLoopback, Wait::Blocking) => vec!["--tcp-loopback: TCP's send path and protocol walk, run in software with no card: 10-30 us,",
                                                    "40,000-120,000 cycles at 4 GHz, about twice --unix."],
        (Mode::Unix | Mode::TcpLoopback, _) => vec!["The chapter gives no number for spinning on loopback: the two wake-ups go, the system",
                                                    "calls, the copies and (for TCP) the protocol walk stay, so expect well under the blocking rung.",
                                                    "Loopback and Unix sockets have no card's ring to poll: --busy-poll measures --spin here."],
        (Mode::Tcp(_) | Mode::Echo(_), Wait::Blocking) => vec!["--tcp <host>: each way a send of 2-5 us, under a microsecond through the switch and 6-22 us",
                                                               "from the wire to recv returning in a sleeping reader: about 17-55 us a round trip, the",
                                                               "ladder's 20-50 us (80,000-200,000 cycles at 4 GHz) and twice loopback."],
        (Mode::Tcp(_) | Mode::Echo(_), Wait::Spin) => vec!["--tcp <host> --spin (both ends): both wake-ups gone; a few us above --busy-poll, since it still",
                                                           "waits for the interrupt, the softirq on another core and the socket's lines to cross.",
                                                           "The ladder puts both ends busy-polling at 8-15 us, 32,000-60,000 cycles at 4 GHz."],
        (Mode::Tcp(_) | Mode::Echo(_), Wait::BusyPoll) => vec!["--tcp <host> --busy-poll (both ends): the driver's poll and the protocol walk on the reader's own",
                                                               "core: the lower half of 8-15 us (32,000-60,000 cycles at 4 GHz). Bypass would be 3-6 us."],
    };
    p.push("Ratios: about 1 threads to processes, 25-150 up to a Unix socket, 2 up to loopback, 2 again to the second");
    p.push("machine, then about 3 down by busy-polling. Blocking rungs: p99 far above p50 (the wake-up varies);");
    p.push("spinning rungs: p99 within a few us of p50.");
    p
}

/// The system calls the chapter expects per round trip, to check with strace -c in a run of its own.
fn expected_calls(a: &Args) -> &'static str {
    match (&a.mode, a.wait) {
        (Mode::Threads | Mode::Shm, _) => "none",
        (_, Wait::Blocking) => "a send and a receive a side",
        _ => "a send, one receive that returns data, and tens to a hundred that return EAGAIN, a side",
    }
}

// ---------------------------------------------------------------- the atomic rungs

/// One word on a 128-byte block of its own: nothing else shares its line, nor
/// the neighbouring line the prefetcher pairs with it.
#[repr(C, align(128))]
struct Padded(AtomicU64);

const STOP: u64 = u64::MAX; // odd, so the echo checks for it before answering

/// The echo side: an odd value is a ping; answer with the next even one.
fn atomic_echo(w: &AtomicU64) {
    loop {
        let v = w.load(Acquire);
        if v == STOP { return; }
        if v & 1 == 1 { w.store(v + 1, Release); } else { std::hint::spin_loop(); }
    }
}

fn atomic_client(w: &AtomicU64, a: &Args, tpn: f64) -> Histogram<u64> {
    let mut h = new_hist();
    for i in 0..a.warmup + a.iters {
        let ping = 2 * i + 1;
        let t0 = now();
        w.store(ping, Release);
        while w.load(Acquire) != ping + 1 { std::hint::spin_loop(); }
        let t1 = now();
        if i >= a.warmup { h.saturating_record(ns(t1 - t0, tpn)); }
    }
    w.store(STOP, Release);
    h
}

/// A page from memfd_create, mapped shared: after fork, both processes see the same word.
fn shared_word() -> io::Result<&'static AtomicU64> {
    // SAFETY: the name is a NUL-terminated literal; flags are valid.
    let fd = unsafe { libc::memfd_create(c"ladder".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: fd is the memfd just made; one page.
    if unsafe { libc::ftruncate(fd, 4096) } != 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: a fresh shared mapping of the whole 4 KB file; checked against MAP_FAILED below.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE,
                                libc::MAP_SHARED, fd, 0) };
    if p == libc::MAP_FAILED { return Err(io::Error::last_os_error()); }
    // SAFETY: fd was opened above and the mapping keeps the file alive without it.
    unsafe { libc::close(fd) };
    // SAFETY: page-aligned, zero-filled, never unmapped, and every bit pattern is a valid AtomicU64.
    Ok(unsafe { &*(p as *const AtomicU64) })
}

// ---------------------------------------------------------------- the socket rungs

/// One byte out, finishing a WouldBlock on a non-blocking socket.
fn send1(s: &mut impl Write, b: u8) -> io::Result<()> {
    loop {
        match s.write(&[b]) {
            Ok(1) => return Ok(()),
            Ok(_) => return Err(ErrorKind::WriteZero.into()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// The echo side: read one byte, write it back, until the client closes.
fn echo(s: &mut (impl Read + Write)) -> io::Result<u64> {
    let mut b = [0u8; 1];
    let mut n = 0;
    loop {
        match s.read(&mut b) {
            Ok(0) => return Ok(n),
            Ok(_) => { send1(s, b[0])?; n += 1; }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

struct Counts { empties: u64, voluntary: Option<u64> }

/// The client: send, then wait for the reply as the socket's mode says,
/// blocking in recv or looping on WouldBlock; time from before the send to
/// after the reply, on this side's TSC only.
fn ping_pong(s: &mut (impl Read + Write), a: &Args, tpn: f64) -> io::Result<(Histogram<u64>, Counts)> {
    let mut h = new_hist();
    let mut b = [0u8; 1];
    let mut empties = 0u64;
    let mut v0 = None;
    for i in 0..a.warmup + a.iters {
        if i == a.warmup { empties = 0; v0 = ctxt_switches(); }
        let t0 = now();
        send1(s, i as u8)?;
        loop {
            match s.read(&mut b) {
                Ok(1) => break,
                Ok(_) => return Err(ErrorKind::UnexpectedEof.into()),
                Err(e) if e.kind() == ErrorKind::WouldBlock => { empties += 1; std::hint::spin_loop(); }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let t1 = now();
        if i >= a.warmup { h.saturating_record(ns(t1 - t0, tpn)); }
    }
    let voluntary = v0.zip(ctxt_switches()).map(|((v0, _), (v1, _))| v1 - v0);
    Ok((h, Counts { empties, voluntary }))
}

/// Puts a socket in the run's mode: blocking as it starts, or non-blocking, or
/// non-blocking with SO_BUSY_POLL. Returns whether busy polling was granted.
fn set_mode<S: AsRawFd>(s: &S, nonblocking: impl Fn(&S) -> io::Result<()>, w: Wait, who: &str) -> io::Result<bool> {
    if w == Wait::Blocking { return Ok(false); }
    nonblocking(s)?;
    Ok(w == Wait::BusyPoll && busy_poll_or_say(s, who))
}

/// Forks the echo side onto the echo core; the parent runs the client on its own.
fn fork_pair<S: Read + Write>(a: &Args, tpn: f64, mut client: S, mut server: S) -> io::Result<(Histogram<u64>, Counts)> {
    match sys::fork()? {
        0 => {
            sys::die_with_parent();
            drop(client);
            pin_or_say(a.echo_core, "echo");
            let code = match echo(&mut server) { Ok(_) => 0, Err(e) => { eprintln!("echo: {e}"); 1 } };
            sys::exit_child(code)
        }
        child => {
            drop(server);
            pin_or_say(a.client_core, "client");
            let r = ping_pong(&mut client, a, tpn);
            drop(client); // the echo reads 0 and exits
            if !sys::wait_child(child) { eprintln!("note: the echo process did not exit cleanly"); }
            r
        }
    }
}

fn resolve(host: &str, port: u16) -> std::net::SocketAddr {
    let target = if host.contains(':') { host.to_string() } else { format!("{host}:{port}") };
    target.to_socket_addrs().ok().and_then(|mut v| v.next())
        .unwrap_or_else(|| usage(&format!("--tcp {host}: cannot resolve {target}")))
}

/// `ladder --echo`: serve one client at a time, for ever.
fn serve(a: &Args) -> ! {
    let Mode::Echo(listen) = &a.mode else { unreachable!() };
    let addr = if listen.is_empty() { format!("0.0.0.0:{}", a.port) }
               else if listen.contains(':') { listen.clone() } else { format!("0.0.0.0:{listen}") };
    let l = TcpListener::bind(&addr).unwrap_or_else(|e| usage(&format!("cannot listen on {addr}: {e}")));
    pin_or_say(a.echo_core, "echo");
    println!("ladder --echo: listening on {addr}, {}; Ctrl-C to stop",
             match a.wait { Wait::Blocking => "blocking reads", Wait::Spin => "spinning on recv", Wait::BusyPoll => "busy-polling" });
    loop {
        let (mut s, peer) = match l.accept() { Ok(c) => c, Err(e) => { eprintln!("accept: {e}"); continue } };
        let _ = s.set_nodelay(true);
        if let Err(e) = set_mode(&s, |s| s.set_nonblocking(true), a.wait, "echo") { eprintln!("{peer}: {e}"); continue; }
        match echo(&mut s) {
            Ok(n) => println!("{peer}: {} round trips served", commas(n)),
            Err(e) => eprintln!("{peer}: {e}"),
        }
    }
}

fn main() {
    let a = parse_args();
    if matches!(a.mode, Mode::Echo(_)) { serve(&a); }

    timing::predict(&prediction(&a));
    let tpn = calibrate(); // single-threaded, before any fork
    println!("ladder {}: {}, {} round trips after {} of warm-up", label(&a), sys::describe_box(tpn),
             commas(a.iters), commas(a.warmup));
    let busy0 = counters(&["TcpExtBusyPollRxPackets"])[0];

    let result: io::Result<(Histogram<u64>, Option<Counts>)> = match &a.mode {
        Mode::Threads => {
            let w = Box::leak(Box::new(Padded(AtomicU64::new(0))));
            let w: &'static AtomicU64 = &w.0;
            let ec = a.echo_core;
            let t = std::thread::spawn(move || { pin_or_say(ec, "echo"); atomic_echo(w) });
            pin_or_say(a.client_core, "client");
            let h = atomic_client(w, &a, tpn);
            let _ = t.join();
            Ok((h, None))
        }
        Mode::Shm => shared_word().and_then(|w| match sys::fork()? {
            0 => { sys::die_with_parent(); pin_or_say(a.echo_core, "echo"); atomic_echo(w); sys::exit_child(0) }
            child => {
                pin_or_say(a.client_core, "client");
                let h = atomic_client(w, &a, tpn);
                if !sys::wait_child(child) { eprintln!("note: the echo process did not exit cleanly"); }
                Ok((h, None))
            }
        }),
        Mode::Unix => UnixStream::pair().and_then(|(c, s)| {
            set_mode(&c, |s| s.set_nonblocking(true), a.wait, "client")?;
            set_mode(&s, |s| s.set_nonblocking(true), a.wait, "echo")?;
            fork_pair(&a, tpn, c, s).map(|(h, k)| (h, Some(k)))
        }),
        Mode::TcpLoopback => TcpListener::bind("127.0.0.1:0").and_then(|l| {
            let c = TcpStream::connect(l.local_addr()?)?;
            let (s, _) = l.accept()?;
            for x in [&c, &s] { x.set_nodelay(true)?; }
            set_mode(&c, |s| s.set_nonblocking(true), a.wait, "client")?;
            set_mode(&s, |s| s.set_nonblocking(true), a.wait, "echo")?;
            fork_pair(&a, tpn, c, s).map(|(h, k)| (h, Some(k)))
        }),
        Mode::Tcp(host) => {
            let addr = resolve(host, a.port);
            TcpStream::connect(addr).and_then(|mut c| {
                c.set_nodelay(true)?;
                set_mode(&c, |s| s.set_nonblocking(true), a.wait, "client")?;
                pin_or_say(a.client_core, "client");
                ping_pong(&mut c, &a, tpn).map(|(h, k)| (h, Some(k)))
            }).map_err(|e| io::Error::new(e.kind(), format!("{addr}: {e} (is `ladder --echo` running there?)")))
        }
        Mode::Echo(_) => unreachable!(),
    };
    let (h, k) = result.unwrap_or_else(|e| { eprintln!("ladder: {e}"); exit(1) });

    timing::report(&format!("ladder {}", label(&a)), &h);
    let p50 = h.value_at_quantile(0.50);
    println!("  p50 = {} TSC ticks at {tpn:.2} GHz = {} cycles at the chapter's 4 GHz",
             commas((p50 as f64 * tpn) as u64), commas(p50 * 4));
    if let Some(k) = k {
        let per = |x: u64| x as f64 / a.iters as f64;
        println!("  per round trip, client side: {:.2} empty polls (EAGAIN), {} voluntary context switches",
                 per(k.empties), k.voluntary.map(|v| format!("{:.3}", per(v))).unwrap_or_else(|| "?".into()));
    }
    if a.wait == Wait::BusyPoll {
        let busy1 = counters(&["TcpExtBusyPollRxPackets"])[0];
        match busy0.zip(busy1) {
            Some((b0, b1)) => println!("  TcpExtBusyPollRxPackets rose by {} ({:.2} a round trip, this box): \
                                        zero means the driver or the path has no ring to poll", commas(b1 - b0),
                                       (b1 - b0) as f64 / (a.iters + a.warmup) as f64),
            None => println!("  TcpExtBusyPollRxPackets: not in /proc/net/netstat here"),
        }
    }
    println!("  system calls per round trip, the chapter's count: {} (check with strace -c -f in a separate run)",
             expected_calls(&a));
}

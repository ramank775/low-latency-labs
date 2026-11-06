//! pollwait: Lab 4.3, poll vs wait. An echo client and server for 100-byte
//! messages, the size of the chapter's depth update, over UDP, or over TCP with
//! --tcp. The server never changes: a non-blocking recv loop with SO_BUSY_POLL
//! on its own core. Only the client's way of waiting differs between runs.
//!
//!     pollwait --echo [--tcp] [--port N] [--core N]
//!     pollwait --host <host>[:port] | --loopback   --blocking | --epoll | --spin | --busy-poll
//!              [--tcp] [--gap US] [--c0] [--core N] [--iters N] [--warmup N]

use std::io::{self, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::AsRawFd;
use std::process::exit;
use std::time::{Duration, Instant};

use ch04_network::sys::{self, busy_poll_or_say, counters, ctxt_switches, pin_or_say, thread_cpu_ns};
use ch04_network::timing::{self, calibrate, commas, human, new_hist, now, ns};
use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};

const MSG: usize = 100;
const TIMEOUT: Duration = Duration::from_millis(100);

const HELP: &str = "\
pollwait: Lab 4.3, poll vs wait: an echo of 100-byte messages over UDP (TCP with --tcp); the server never
changes, so only the client's way of waiting differs. p50 / p99 / max round trip, the client's CPU time,
context switches and empty polls.

usage:
  pollwait --echo [--tcp] [--port N] [--core N]           the server: non-blocking recv loop, SO_BUSY_POLL
  pollwait --host <host>[:port] <mode> [options]          the client, against --echo on the second machine
  pollwait --loopback <mode> [options] [--server-core N]  the client, with the server forked here

modes:
  --blocking     a blocking recv
  --epoll        the socket registered with mio, Poll::poll, then drained to WouldBlock
  --spin         a non-blocking recv in a loop
  --busy-poll    the same with SO_BUSY_POLL at 50 us (a kernel that refuses it without CAP_NET_ADMIN
                 gets a message, and the run measures --spin)

options:
  --tcp          TCP instead of UDP (give it to the server too)
  --gap US       the server holds each reply this long, spinning, and the client subtracts it: the wait
                 between updates of a quiet feed (default 0; the lab runs --gap 2000, 500 messages a second)
  --c0           hold /dev/cpu_dma_latency at 0 for the run (Chapter 2's tool; needs root)
  --core N       the client's core (default 3); with --echo, the server's (default 2)
  --server-core N  the forked server's core with --loopback (default 2)
  --iters N      timed round trips (default 100,000)
  --warmup N     round trips before timing (default 10,000)
  --port N       the server's port (default 9002)

The lab: all four modes with and without --c0 at --gap 2000, then --blocking once more without --c0 at
--gap 0. Every message carries a sequence number and every wait a 100 ms timeout; losses are counted and
left out of the histogram. Count calls in a separate run: strace -c -f ./target/release/pollwait ...;
the spinning modes' empty polls: perf stat -e syscalls:sys_enter_recvfrom; CPU share: top -H or
perf stat -e task-clock; C-state residency: turbostat.";

#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode { Blocking, Epoll, Spin, BusyPoll }

struct Args {
    mode: Option<Mode>,
    host: Option<String>,
    loopback: bool,
    echo: bool,
    tcp: bool,
    gap_us: u64,
    c0: bool,
    core: Option<Option<usize>>,
    server_core: Option<usize>,
    iters: u64,
    warmup: u64,
    port: u16,
}

fn usage(msg: &str) -> ! {
    eprintln!("pollwait: {msg}\n\n{HELP}");
    exit(2)
}

fn parse_args() -> Args {
    let mut a = Args { mode: None, host: None, loopback: false, echo: false, tcp: false, gap_us: 0, c0: false,
                       core: None, server_core: Some(2), iters: 100_000, warmup: 10_000, port: 9002 };
    let core = |v: String| if v == "none" { None } else { Some(v.parse().unwrap_or_else(|_| usage("a core number or none"))) };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        let mut set = |m: Mode| { if a.mode.is_some() { usage("give one mode"); } a.mode = Some(m); };
        match flag.as_str() {
            "--blocking" => set(Mode::Blocking),
            "--epoll" => set(Mode::Epoll),
            "--spin" => set(Mode::Spin),
            "--busy-poll" => set(Mode::BusyPoll),
            "--host" => a.host = Some(val()),
            "--loopback" => a.loopback = true,
            "--echo" => a.echo = true,
            "--tcp" => a.tcp = true,
            "--c0" => a.c0 = true,
            "--gap" => a.gap_us = timing::count(&val()).unwrap_or_else(|| usage("--gap: microseconds")),
            "--core" => a.core = Some(core(val())),
            "--server-core" => a.server_core = core(val()),
            "--iters" => a.iters = timing::count(&val()).unwrap_or_else(|| usage("--iters: a count")),
            "--warmup" => a.warmup = timing::count(&val()).unwrap_or_else(|| usage("--warmup: a count")),
            "--port" => a.port = val().parse().unwrap_or_else(|_| usage("--port: a port number")),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if [a.echo, a.loopback, a.host.is_some()].iter().filter(|&&x| x).count() != 1 {
        usage("give one of --echo, --loopback or --host <host>");
    }
    if !a.echo && a.mode.is_none() { usage("give a mode: --blocking, --epoll, --spin or --busy-poll"); }
    if a.iters == 0 { usage("--iters must be at least 1"); }
    a
}

fn prediction(a: &Args) -> Vec<String> {
    let m = a.mode.unwrap_or(Mode::Blocking);
    let mut p = vec!["Two machines (the chapter's setup): the server's half and the wire are the same in every run, the".to_string(),
                     "ladder's busy-polled round trip less one busy-polled receive, about 5-12 us. Each mode adds its own receive:".into()];
    let quiet = a.gap_us >= 1_000;
    match (m, a.c0) {
        (Mode::BusyPoll, _) => p.push("--busy-poll: about 3 us at p50 and 5-6 us at p99, round trips of 8-15 us; --c0 changes nothing.".into()),
        (Mode::Spin, true) => p.push("--spin --c0: 1-2 us more than --busy-poll, for the interrupt and the cross-core transfer.".into()),
        (Mode::Spin, false) => p.push("--spin without --c0: not immune; its softirq runs on a housekeeping core that now sleeps between packets, \
                                       and many round trips pay that core's C6 exit, 100-200 us.".into()),
        (Mode::Epoll, true) => p.push("--epoll --c0: the card and the kernel's receive path, 8-10 us at p50 and 20-40 us at p99: round trips \
                                       of 13-22 us at p50 and 25-50 us at p99.".into()),
        (Mode::Blocking, true) => p.push("--blocking --c0: --epoll's numbers less the epoll_wait, about 0.5 us: p50 13-22 us, p99 25-50 us.".into()),
        (Mode::Epoll | Mode::Blocking, false) if quiet => p.push("a sleeping mode without --c0 at a 2 ms gap: the governor picks its deepest state, +100-200 us: \
                                                                  p50 105-210 us, p99 several hundred, max in ms.".into()),
        (Mode::Epoll | Mode::Blocking, false) => p.push("a sleeping mode at --gap 0: near the --c0 numbers (p50 13-22 us), since a core idle for 15-30 us \
                                                         is never sent deep: quiet sets a sleeping reader's tail.".into()),
    }
    match m {
        Mode::Spin | Mode::BusyPoll => p.push("CPU: 100% at any gap; at 100-300 ns an empty poll, 3-10 million polls a second, about 7,000-20,000 \
                                               per round trip at --gap 2000. System calls: only recvfrom (and the send).".into()),
        Mode::Epoll => p.push("CPU: near 0%, 1% at most. One voluntary context switch a round trip. System calls: one epoll_wait and \
                               two recvfrom, one of them EAGAIN (and the send).".into()),
        Mode::Blocking => p.push("CPU: near 0%, 1% at most. One voluntary context switch a round trip. System calls: one recvfrom \
                                  (and the send).".into()),
    }
    if a.loopback {
        p.push("Over --loopback there is no card, no interrupt and no second box: the sender's own system call runs the".into());
        p.push("receive path, so the modes differ by the wake-up alone, and --busy-poll has no ring to poll (it measures --spin).".into());
    }
    p
}

// ---------------------------------------------------------------- the wire

enum Sock { Udp(UdpSocket), Tcp(TcpStream) }

impl Sock {
    fn fd(&self) -> i32 { match self { Sock::Udp(s) => s.as_raw_fd(), Sock::Tcp(s) => s.as_raw_fd() } }

    fn set_nonblocking(&self, on: bool) -> io::Result<()> {
        match self { Sock::Udp(s) => s.set_nonblocking(on), Sock::Tcp(s) => s.set_nonblocking(on) }
    }

    fn set_read_timeout(&self, t: Option<Duration>) -> io::Result<()> {
        match self { Sock::Udp(s) => s.set_read_timeout(t), Sock::Tcp(s) => s.set_read_timeout(t) }
    }

    /// One message out, finishing a WouldBlock on a non-blocking socket.
    fn send(&mut self, m: &[u8; MSG]) -> io::Result<()> {
        let mut at = 0;
        while at < MSG {
            let r = match self { Sock::Udp(s) => s.send(m), Sock::Tcp(s) => s.write(&m[at..]) };
            match r {
                Ok(n) => at += if matches!(self, Sock::Udp(_)) { MSG } else { n },
                Err(e) if e.kind() == ErrorKind::WouldBlock => std::hint::spin_loop(),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// One receive call. True when a whole message is in `rx.buf`; a TCP read
    /// that ends mid-message keeps what it has in `rx.got`.
    fn recv_once(&mut self, rx: &mut Rx) -> io::Result<bool> {
        match self {
            Sock::Udp(s) => { let n = s.recv(&mut rx.buf)?; Ok(n == MSG) }
            Sock::Tcp(s) => {
                let n = s.read(&mut rx.buf[rx.got..])?;
                if n == 0 { return Err(ErrorKind::UnexpectedEof.into()); }
                rx.got += n;
                if rx.got == MSG { rx.got = 0; Ok(true) } else { Ok(false) }
            }
        }
    }
}

struct Rx { buf: [u8; MSG], got: usize }

fn seq_of(b: &[u8; MSG]) -> u64 { u64::from_le_bytes(b[0..8].try_into().unwrap()) }

/// Spins for `ns` nanoseconds on the TSC: the gap a quiet feed leaves.
fn spin_ns(ns: u64, tpn: f64) {
    if ns == 0 { return; }
    let until = now() + (ns as f64 * tpn) as u64;
    while now() < until { std::hint::spin_loop(); }
}

// ---------------------------------------------------------------- the server

fn serve_udp(sock: UdpSocket, tpn: f64) -> io::Result<()> {
    sock.set_nonblocking(true)?;
    busy_poll_or_say(&sock, "server");
    let mut buf = [0u8; 2048];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, peer)) => {
                if n >= 16 { spin_ns(u64::from_le_bytes(buf[8..16].try_into().unwrap()), tpn); }
                let _ = sock.send_to(&buf[..n], peer);
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => eprintln!("server: {e}"),
        }
    }
}

fn serve_tcp_conn(s: TcpStream, tpn: f64) -> io::Result<u64> {
    s.set_nodelay(true)?;
    s.set_nonblocking(true)?;
    busy_poll_or_say(&s, "server");
    let mut sock = Sock::Tcp(s);
    let mut rx = Rx { buf: [0; MSG], got: 0 };
    let mut n = 0;
    loop {
        match sock.recv_once(&mut rx) {
            Ok(true) => {
                spin_ns(u64::from_le_bytes(rx.buf[8..16].try_into().unwrap()), tpn);
                let m = rx.buf;
                sock.send(&m)?;
                n += 1;
            }
            Ok(false) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::hint::spin_loop(),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => return Ok(n),
            Err(e) => return Err(e),
        }
    }
}

fn serve(tcp: bool, port: u16, tpn: f64, ready: Option<std::os::unix::net::UnixStream>) -> ! {
    let addr = if ready.is_some() { format!("127.0.0.1:{port}") } else { format!("0.0.0.0:{port}") };
    let done = |r: io::Result<()>| -> ! { if let Err(e) = r { eprintln!("server: {e}"); sys::exit_child(1) } sys::exit_child(0) };
    if tcp {
        let l = TcpListener::bind(&addr).unwrap_or_else(|e| { eprintln!("server: listen {addr}: {e}"); sys::exit_child(1) });
        say_ready(ready, l.local_addr().map(|a| a.port()).unwrap_or(0), tcp);
        loop {
            let Ok((s, peer)) = l.accept() else { continue };
            match serve_tcp_conn(s, tpn) {
                Ok(n) => println!("server: {peer}: {} round trips served", commas(n)),
                Err(e) => eprintln!("server: {peer}: {e}"),
            }
        }
    } else {
        let s = UdpSocket::bind(&addr).unwrap_or_else(|e| { eprintln!("server: bind {addr}: {e}"); sys::exit_child(1) });
        say_ready(ready, s.local_addr().map(|a| a.port()).unwrap_or(0), tcp);
        done(serve_udp(s, tpn))
    }
}

/// The forked server tells the client its port; a standalone one says where it listens.
fn say_ready(ready: Option<std::os::unix::net::UnixStream>, port: u16, tcp: bool) {
    match ready {
        Some(mut r) => { let _ = r.write_all(&port.to_le_bytes()); }
        None => println!("pollwait --echo: {} on port {port}, non-blocking recv loop; Ctrl-C to stop",
                         if tcp { "TCP" } else { "UDP" }),
    }
}

// ---------------------------------------------------------------- the client

#[derive(Default)]
struct Tally { lost: u64, empties: u64, waits: u64 }

/// Waits for the reply to `seq` the run's way. True when it came, false on a
/// 100 ms timeout (a loss). Stale replies, to messages already given up on, are skipped.
fn wait(sock: &mut Sock, rx: &mut Rx, seq: u64, mode: Mode, poll: &mut Option<(Poll, Events)>,
        tally: &mut Tally, timeout_ticks: u64) -> io::Result<bool> {
    let t0 = now();
    match mode {
        Mode::Blocking => loop {
            match sock.recv_once(rx) {
                Ok(true) if seq_of(&rx.buf) == seq => return Ok(true),
                Ok(_) => {}
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    if matches!(sock, Sock::Udp(_)) { return Ok(false); } // TCP never loses: keep waiting
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        },
        Mode::Epoll => {
            let (p, events) = poll.as_mut().expect("registered at startup");
            let mut got = false;
            loop {
                p.poll(events, Some(TIMEOUT))?;   // epoll_wait: sleep until ready, or the loss timeout
                tally.waits += 1;
                loop {                             // edge-triggered: drain to WouldBlock
                    match sock.recv_once(rx) {
                        Ok(true) if seq_of(&rx.buf) == seq => got = true,
                        Ok(_) => {}
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) if e.kind() == ErrorKind::Interrupted => {}
                        Err(e) => return Err(e),
                    }
                }
                if got { return Ok(true); }
                if now() - t0 > timeout_ticks && matches!(sock, Sock::Udp(_)) { return Ok(false); }
            }
        }
        Mode::Spin | Mode::BusyPoll => loop {
            match sock.recv_once(rx) {
                Ok(true) if seq_of(&rx.buf) == seq => return Ok(true),
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    tally.empties += 1;
                    if matches!(sock, Sock::Udp(_)) && now() - t0 > timeout_ticks { return Ok(false); }
                    std::hint::spin_loop(); // nothing queued: ask again
                }
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        },
    }
}

fn connect(a: &Args, to: SocketAddr) -> io::Result<Sock> {
    if a.tcp {
        let s = TcpStream::connect(to)?;
        s.set_nodelay(true)?;
        Ok(Sock::Tcp(s))
    } else {
        let s = UdpSocket::bind(if to.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
        s.connect(to)?;
        Ok(Sock::Udp(s))
    }
}

fn main() {
    let a = parse_args();
    let tpn = calibrate(); // before any fork: the server inherits it

    if a.echo {
        pin_or_say(a.core.unwrap_or(Some(2)), "server");
        serve(a.tcp, a.port, tpn, None);
    }
    let mode = a.mode.expect("checked in parse_args");
    timing::predict(&prediction(&a).iter().map(String::as_str).collect::<Vec<_>>());

    // --loopback: fork the server, which reports its port over a socket pair
    let (to, child) = if a.loopback {
        let (mut r, w) = std::os::unix::net::UnixStream::pair().unwrap_or_else(|e| usage(&format!("socketpair: {e}")));
        match sys::fork() {
            Ok(0) => { sys::die_with_parent(); drop(r); pin_or_say(a.server_core, "server"); serve(a.tcp, 0, tpn, Some(w)) }
            Ok(pid) => {
                drop(w);
                let mut p = [0u8; 2];
                r.read_exact(&mut p).unwrap_or_else(|e| usage(&format!("the forked server did not start: {e}")));
                (SocketAddr::from(([127, 0, 0, 1], u16::from_le_bytes(p))), Some(pid))
            }
            Err(e) => usage(&format!("fork: {e}")),
        }
    } else {
        let h = a.host.clone().unwrap_or_default();
        let target = if h.contains(':') { h.clone() } else { format!("{h}:{}", a.port) };
        let to = target.to_socket_addrs().ok().and_then(|mut v| v.next())
            .unwrap_or_else(|| usage(&format!("--host {h}: cannot resolve {target}")));
        (to, None)
    };

    let mut sock = connect(&a, to).unwrap_or_else(|e| usage(&format!("{to}: {e} (is `pollwait --echo{}` running there?)",
                                                                       if a.tcp { " --tcp" } else { "" })));
    let setup = (|| -> io::Result<Option<(Poll, Events)>> {
        match mode {
            Mode::Blocking => { sock.set_read_timeout(Some(TIMEOUT))?; Ok(None) }
            Mode::Epoll => {
                sock.set_nonblocking(true)?;
                let p = Poll::new()?;                                   // epoll_create1
                p.registry().register(&mut SourceFd(&sock.fd()), Token(0), Interest::READABLE)?; // EPOLLET
                Ok(Some((p, Events::with_capacity(64))))
            }
            Mode::Spin => { sock.set_nonblocking(true)?; Ok(None) }
            Mode::BusyPoll => {
                sock.set_nonblocking(true)?;
                busy_poll_or_say(&sock.fd(), "client");
                Ok(None)
            }
        }
    })();
    let mut poll = setup.unwrap_or_else(|e| usage(&format!("setting up the socket: {e}")));
    pin_or_say(a.core.unwrap_or(Some(3)), "client");
    let _c0 = if a.c0 {
        match sys::hold_c0() {
            Ok(f) => { println!("--c0: /dev/cpu_dma_latency held at 0 for the run"); Some(f) }
            Err(e) => { eprintln!("note: --c0: {e}: running without it"); None }
        }
    } else { None };

    let label = format!("pollwait --{}{}{} --gap {}", format!("{mode:?}").to_lowercase().replace("busypoll", "busy-poll"),
                        if a.tcp { " --tcp" } else { "" }, if a.c0 { " --c0" } else { "" }, a.gap_us);
    println!("{label} to {to}: {}, {} round trips after {} of warm-up", sys::describe_box(tpn),
             commas(a.iters), commas(a.warmup));

    let gap_ns = a.gap_us * 1_000;
    let timeout_ticks = (TIMEOUT.as_nanos() as f64 * tpn) as u64;
    let mut h = new_hist();
    let mut rx = Rx { buf: [0; MSG], got: 0 };
    let mut msg = [0u8; MSG];
    msg[8..16].copy_from_slice(&gap_ns.to_le_bytes());
    let mut tally = Tally::default();
    const NAMES: [&str; 2] = ["TcpExtBusyPollRxPackets", "UdpRcvbufErrors"];
    let (mut cpu0, mut wall0, mut sw0, mut k0) = (0, Instant::now(), None, counters(&NAMES));
    let run = (|| -> io::Result<()> {
        for i in 0..a.warmup + a.iters {
            if i == a.warmup {
                tally = Tally::default();
                (cpu0, wall0, sw0, k0) = (thread_cpu_ns(), Instant::now(), ctxt_switches(), counters(&NAMES));
            }
            let seq = i + 1;
            msg[0..8].copy_from_slice(&seq.to_le_bytes());
            let t0 = now();
            sock.send(&msg)?;
            let ok = wait(&mut sock, &mut rx, seq, mode, &mut poll, &mut tally, timeout_ticks)?;
            let t1 = now();
            if !ok { tally.lost += 1; continue; }
            if i >= a.warmup { h.saturating_record(ns(t1 - t0, tpn).saturating_sub(gap_ns).max(1)); }
        }
        Ok(())
    })();
    let (cpu, wall) = (thread_cpu_ns() - cpu0, wall0.elapsed().as_nanos() as u64);
    let (sw1, k1) = (ctxt_switches(), counters(&NAMES));
    drop(_c0);
    if let Some(pid) = child {
        // SAFETY: kill sends a signal to the server forked above, which spins for ever.
        unsafe { libc::kill(pid, libc::SIGTERM) };
        sys::wait_child(pid);
    }
    if let Err(e) = run { eprintln!("pollwait: {e}"); exit(1) }

    timing::report(&label, &h);
    let per = |x: u64| x as f64 / a.iters as f64;
    println!("  client CPU {} over {} of wall: {:.1}% of its core", human(cpu), human(wall), 100.0 * cpu as f64 / wall.max(1) as f64);
    if let (Some((v0, n0)), Some((v1, n1))) = (sw0, sw1) {
        println!("  per round trip: {:.3} voluntary and {:.3} involuntary context switches", per(v1 - v0), per(n1 - n0));
    }
    match mode {
        Mode::Spin | Mode::BusyPoll => println!("  empty polls: {:.0} a round trip, {} a second", per(tally.empties),
                                                commas((tally.empties as f64 / (wall as f64 / 1e9)) as u64)),
        Mode::Epoll => println!("  epoll_wait calls: {:.2} a round trip", per(tally.waits)),
        Mode::Blocking => {}
    }
    let delta = |k: usize| k0[k].zip(k1[k]).map(|(x, y)| y - x);
    println!("  lost (100 ms timeout): {}; UdpRcvbufErrors +{}; TcpExtBusyPollRxPackets +{} (box-wide)", tally.lost,
             delta(1).map(commas).unwrap_or_else(|| "?".into()), delta(0).map(commas).unwrap_or_else(|| "?".into()));
}

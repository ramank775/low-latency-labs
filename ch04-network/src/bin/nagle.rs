//! nagle: Lab 4.2, Nagle. A request is 48 bytes, a 4-byte header holding the
//! length and a 44-byte body; the client writes each as two write_all calls,
//! header then body, and waits for its echo before the next. The echo server
//! reads until it holds a whole request and writes it back in one call: like a
//! venue, it never answers half a message.
//!
//!     nagle --loopback | --host <host>[:port]  [--nodelay] [--one-write] [--cork]
//!     nagle --echo [--port N] [--quickack]

use std::io::{self, ErrorKind, IoSlice, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::os::linux::net::TcpStreamExt;
use std::process::exit;
use std::time::Instant;

use ch04_network::sys::{self, counters};
use ch04_network::timing::{self, calibrate, commas, human, new_hist, now, ns};
use socket2::SockRef;

const HELP: &str = "\
nagle: Lab 4.2, Nagle: 1,000 requests of 48 bytes, a 4-byte header holding the length then a 44-byte
body, each written as two write_all calls and each waiting for its echo; total time and p50 / p99 / max.

usage:
  nagle --loopback [variant]           forks the echo server here, on 127.0.0.1
  nagle --host <host>[:port] [variant] against `nagle --echo` on the second machine
  nagle --echo [--port N] [--quickack] the server: reads a whole request, writes it back in one call

variants (the default is two write_all calls with Nagle on, as every socket starts):
  --nodelay      TCP_NODELAY on the client's socket; still two writes
  --one-write    header and body in one write_vectored call
  --cork         TCP_CORK set before the header and cleared after the body

options:
  --requests N   timed requests (default 1,000; the default variant takes about 40 ms each)
  --warmup N     requests before timing (default 100: a new connection quick-acks its first few)
  --port N       the server's port for --host and --echo (default 9001)
  --quickack     the server sets TCP_QUICKACK after every read (with --loopback, the forked one):
                 the stall goes, which shows whose timer it is; no venue will set it for anyone

See the stall: tcpdump -ttt -nn -i lo port <port> shows a 4-byte segment, an empty ACK about 0.040 s
later, and the body just after. ss -ti on the server shows ato:40. Count calls in a separate run:
strace -c -f ./target/release/nagle --loopback --requests 100   (Rust's TcpStream makes sendto, writev, recvfrom)";

#[derive(Clone, Copy, Default)]
struct Variant { nodelay: bool, one_write: bool, cork: bool }

fn usage(msg: &str) -> ! {
    eprintln!("nagle: {msg}\n\n{HELP}");
    exit(2)
}

fn name(v: Variant) -> String {
    let mut s = Vec::new();
    if v.nodelay { s.push("--nodelay"); }
    if v.one_write { s.push("--one-write"); }
    if v.cork { s.push("--cork"); }
    if s.is_empty() { "default (two writes, Nagle on)".into() } else { s.join(" ") }
}

fn prediction(v: Variant) -> Vec<&'static str> {
    let mut p = if !v.nodelay && !v.one_write && !v.cork {
        vec!["default: the header leaves at once, nothing being in flight; Nagle holds the body, the header being",
             "unacknowledged. The server holds half a request and has no reply to carry an ACK, so it waits out its",
             "40 ms delayed-ACK timer: about 40 ms a request and 40 s for the 1,000, on loopback and across a switch alike.",
             "System calls a request: two sendto and one recvfrom."]
    } else {
        vec!["With any fix a request is one ordinary round trip, 10-30 us on loopback and 20-50 us across a switch:",
             "the 1,000 take 10-50 ms, eight hundred to four thousand times less than the default."]
    };
    if v.one_write {
        p.push("--one-write: one segment, and nothing in flight when it is written (the last echo acknowledged every");
        p.push("byte), so it leaves at once. System calls a request: one writev and one recvfrom.");
    } else if v.cork {
        p.push("--cork: the same single segment as --one-write for three more system calls, two setsockopt and a");
        p.push("second send, a microsecond or two a request: two setsockopt, two sendto and one recvfrom.");
    } else if v.nodelay {
        p.push("--nodelay: two segments about a microsecond apart (a few more if autocorking holds the body for the");
        p.push("header's transmit completion); a server that wakes on the header reads 4 bytes and sleeps again, so a");
        p.push("p50 a microsecond or two above --one-write's and a p99 up to a wake-up above it. Two sendto, one recvfrom.");
    }
    p
}

/// One request: the 4-byte length, then the body, which starts with the sequence number.
fn request(seq: u64) -> [u8; 48] {
    let mut r = [0u8; 48];
    r[0..4].copy_from_slice(&48u32.to_le_bytes());
    r[4..12].copy_from_slice(&seq.to_le_bytes());
    r
}

fn send(s: &mut TcpStream, v: Variant, req: &[u8; 48]) -> io::Result<()> {
    let (hdr, body) = req.split_at(4);
    if v.one_write {
        let n = s.write_vectored(&[IoSlice::new(hdr), IoSlice::new(body)])?;
        if n < req.len() { s.write_all(&req[n..])?; } // a short count must be finished
    } else if v.cork {
        SockRef::from(&*s).set_tcp_cork(true)?;  // hold partial segments...
        s.write_all(hdr)?;
        s.write_all(body)?;
        SockRef::from(&*s).set_tcp_cork(false)?; // ...and send them now, as one
    } else {
        s.write_all(hdr)?;
        s.write_all(body)?;
    }
    Ok(())
}

/// The server's side of one connection: read until a whole request is held,
/// never answering half of one, then write it back in one call.
fn serve_conn(mut s: TcpStream, quickack: bool) -> io::Result<u64> {
    let mut buf = [0u8; 48];
    let mut served = 0;
    loop {
        let mut got = 0;
        let len = loop {
            if got >= 4 {
                let len = u32::from_le_bytes(buf[0..4].try_into().unwrap()) as usize;
                if !(4..=48).contains(&len) { return Err(ErrorKind::InvalidData.into()); }
                if got >= len { break len; }
            }
            // one request is in flight at a time, so a read never takes the next one's bytes
            let n = s.read(&mut buf[got..])?;
            if n == 0 {
                return if got == 0 { Ok(served) } else { Err(ErrorKind::UnexpectedEof.into()) };
            }
            got += n;
            if quickack { s.set_quickack(true)?; } // a system call after every read
        };
        s.write_all(&buf[..len])?;
        served += 1;
    }
}

fn main() {
    let mut v = Variant::default();
    let (mut loopback, mut host, mut echo, mut quickack) = (false, None::<String>, false, false);
    let (mut requests, mut warmup, mut port) = (1_000u64, 100u64, 9001u16);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--loopback" => loopback = true,
            "--host" => host = Some(val()),
            "--echo" => echo = true,
            "--nodelay" => v.nodelay = true,
            "--one-write" => v.one_write = true,
            "--cork" => v.cork = true,
            "--quickack" => quickack = true,
            "--requests" => requests = timing::count(&val()).unwrap_or_else(|| usage("--requests: a count")),
            "--warmup" => warmup = timing::count(&val()).unwrap_or_else(|| usage("--warmup: a count")),
            "--port" => port = val().parse().unwrap_or_else(|_| usage("--port: a port number")),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }
    if [loopback, host.is_some(), echo].iter().filter(|&&x| x).count() != 1 {
        usage("give one of --loopback, --host <host> or --echo");
    }
    if v.one_write && v.cork { usage("--one-write and --cork are two ways to send one segment: give one"); }
    if requests == 0 { usage("--requests must be at least 1"); }

    if echo {
        let l = TcpListener::bind(("0.0.0.0", port)).unwrap_or_else(|e| usage(&format!("cannot listen on port {port}: {e}")));
        println!("nagle --echo: listening on 0.0.0.0:{port}{}; Ctrl-C to stop", if quickack { ", TCP_QUICKACK after every read" } else { "" });
        loop {
            let Ok((s, peer)) = l.accept() else { continue };
            match serve_conn(s, quickack) {
                Ok(n) => println!("{peer}: {} requests served", commas(n)),
                Err(e) => eprintln!("{peer}: {e}"),
            }
        }
    }

    timing::predict(&prediction(v));
    if quickack { println!("  (server --quickack: the chapter's pitfall; expect the default's stall to vanish)"); }
    let tpn = calibrate();

    let (mut s, child) = if loopback {
        let l = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| usage(&format!("loopback listener: {e}")));
        let addr = l.local_addr().unwrap_or_else(|e| usage(&format!("loopback listener: {e}")));
        match sys::fork() {
            Ok(0) => {
                sys::die_with_parent();
                let code = match l.accept().and_then(|(s, _)| serve_conn(s, quickack)) {
                    Ok(_) => 0, Err(e) => { eprintln!("nagle server: {e}"); 1 }
                };
                sys::exit_child(code)
            }
            Ok(pid) => {
                drop(l);
                (TcpStream::connect(addr).unwrap_or_else(|e| usage(&format!("connect {addr}: {e}"))), Some(pid))
            }
            Err(e) => usage(&format!("fork: {e}")),
        }
    } else {
        let h = host.unwrap_or_default();
        let target = if h.contains(':') { h.clone() } else { format!("{h}:{port}") };
        let addr = target.to_socket_addrs().ok().and_then(|mut a| a.next())
            .unwrap_or_else(|| usage(&format!("--host {h}: cannot resolve {target}")));
        (TcpStream::connect(addr).unwrap_or_else(|e| usage(&format!("{addr}: {e} (is `nagle --echo` running there?)"))), None)
    };
    if v.nodelay { s.set_nodelay(true).unwrap_or_else(|e| usage(&format!("TCP_NODELAY: {e}"))); }
    let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let label = if quickack && loopback { format!("{}, server --quickack", name(v)) } else { name(v) };
    println!("nagle {label} to {peer}: {}, {} requests after {} of warm-up", sys::describe_box(tpn),
             commas(requests), commas(warmup));

    const NAMES: [&str; 4] = ["TcpExtDelayedACKs", "TcpExtTCPAutoCorking", "TcpOutSegs", "TcpRetransSegs"];
    let mut h = new_hist();
    let mut echo_buf = [0u8; 48];
    let (mut c0, mut wall0) = (counters(&NAMES), Instant::now());
    let run = (|| -> io::Result<()> {
        for i in 0..warmup + requests {
            if i == warmup { c0 = counters(&NAMES); wall0 = Instant::now(); }
            let req = request(i);
            let t0 = now();
            send(&mut s, v, &req)?;
            s.read_exact(&mut echo_buf)?;
            let t1 = now();
            if echo_buf != req { return Err(io::Error::new(ErrorKind::InvalidData, "the echo is not the request")); }
            if i >= warmup { h.saturating_record(ns(t1 - t0, tpn)); }
        }
        Ok(())
    })();
    let wall = wall0.elapsed();
    let c1 = counters(&NAMES);
    drop(s);
    if let Some(pid) = child { sys::wait_child(pid); }
    if let Err(e) = run { eprintln!("nagle: {e}"); exit(1) }

    println!("the {} requests took {}", commas(requests), human(wall.as_nanos() as u64));
    timing::report(&format!("nagle {label}"), &h);
    let per = |k: usize| match (c0[k], c1[k]) {
        (Some(a), Some(b)) => format!("{:.2}", (b - a) as f64 / requests as f64),
        _ => "?".into(),
    };
    println!("  per request, box-wide (on loopback both ends are this box): TcpExtDelayedACKs {}, \
              TcpExtTCPAutoCorking {}, TcpOutSegs {} (data and ACKs, both directions), TcpRetransSegs {}",
             per(0), per(1), per(2), per(3));
}

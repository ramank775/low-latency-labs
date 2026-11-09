//! 4.6, waiting for a packet: the epoll loop through mio, the spinning loop on
//! a non-blocking socket, and SO_BUSY_POLL. The chapter's loops never end; the
//! one glue line in each, `more()`, lets a test or a lab stop them.

use mio::net::UdpSocket;
use mio::{Events, Interest, Poll, Token};
use std::io::ErrorKind;
use std::os::fd::AsRawFd;

/// Sleep in epoll_wait until the feed's socket is readable, then drain it to
/// WouldBlock, edge-triggered as mio always registers.
pub fn epoll_feed(addr: &str, mut handle: impl FnMut(&[u8]), mut more: impl FnMut() -> bool)
    -> Result<(), Box<dyn std::error::Error>>
{
    let mut poll = Poll::new()?;                         // epoll_create1
    let mut feed = UdpSocket::bind(addr.parse()?)?; // non-blocking
    poll.registry()                                      // epoll_ctl, EPOLLET
        .register(&mut feed, Token(0), Interest::READABLE)?;
    let mut events = Events::with_capacity(64);
    let mut buf = [0u8; 2048];
    loop {
        poll.poll(&mut events, None)?; // epoll_wait(-1): sleep until ready
        for _ in events.iter() {
            loop {                     // edge-triggered: drain to WouldBlock
                match feed.recv_from(&mut buf) {
                    Ok((n, _)) => handle(&buf[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
        if !more() { return Ok(()); }  // glue: the chapter's loop runs for ever
    }
}

/// Never sleep: a non-blocking recv in a loop on a core the thread has to itself.
pub fn spin_feed(sock: &std::net::UdpSocket, mut handle: impl FnMut(&[u8]), mut more: impl FnMut() -> bool)
    -> std::io::Result<()>
{
    sock.set_nonblocking(true)?; // a std UdpSocket bound to the feed
    let mut buf = [0u8; 2048];
    loop {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => handle(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::hint::spin_loop(); // nothing queued: ask again
            }
            Err(e) => return Err(e),
        }
        if !more() { return Ok(()); }  // glue
    }
}

/// SO_BUSY_POLL at 50 µs: an empty recv runs the driver's poll itself, on this
/// core. Kernels before 6.4 refuse it with EPERM above net.core.busy_read (0 by
/// default) without CAP_NET_ADMIN; `sys::busy_poll_or_say` turns that into a
/// message. Since 6.4 anyone may set it.
pub fn set_busy_poll(sock: &impl AsRawFd) -> std::io::Result<()> {
    let usecs: libc::c_int = 50; // before 6.4, above net.core.busy_read needs CAP_NET_ADMIN
    // SAFETY: the pointer and length describe `usecs`, a live c_int, for the call's duration.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BUSY_POLL,
            &usecs as *const libc::c_int as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
    use std::sync::Arc;
    use std::time::Duration;

    /// Sends one datagram a millisecond to `to` until `stop`, so a receiver
    /// that binds a moment later still gets traffic.
    fn sender(to: String, stop: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let mut seq = 0u64;
            while !stop.load(Relaxed) {
                let _ = tx.send_to(&seq.to_le_bytes(), &to);
                seq += 1;
                std::thread::sleep(Duration::from_millis(1));
            }
        })
    }

    #[test]
    fn the_epoll_loop_receives_and_stops() {
        let stop = Arc::new(AtomicBool::new(false));
        let port = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let t = sender(addr.clone(), stop.clone());
        let got = std::cell::Cell::new(0);
        let r = epoll_feed(&addr, |m| { assert_eq!(m.len(), 8); got.set(got.get() + 1); }, || got.get() < 5);
        stop.store(true, Relaxed);
        t.join().unwrap();
        r.expect("the epoll loop");
        assert!(got.get() >= 5);
    }

    #[test]
    fn the_spinning_loop_receives_and_stops() {
        let stop = Arc::new(AtomicBool::new(false));
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let t = sender(sock.local_addr().unwrap().to_string(), stop.clone());
        let got = std::cell::Cell::new(0);
        spin_feed(&sock, |m| { assert_eq!(m.len(), 8); got.set(got.get() + 1); }, || got.get() < 5)
            .expect("the spin loop");
        stop.store(true, Relaxed);
        t.join().unwrap();
        assert_eq!(got.get(), 5);
    }

    #[test]
    fn busy_poll_is_set_or_refused_for_want_of_a_capability() {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        match set_busy_poll(&sock) {
            Ok(()) => {}
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e}"),
        }
    }
}

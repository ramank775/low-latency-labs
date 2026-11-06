//! 4.10, how the sockets are set up: the hot socket opened once at startup,
//! buffers sized before connect, Nagle off, SO_BUSY_POLL at 50 µs, non-blocking.
//! Where the kernel refuses the busy-poll setsockopt without CAP_NET_ADMIN, `open_hot`
//! returns that error, as the chapter's code does.

use libc::{c_int, setsockopt, socklen_t, SOL_SOCKET, SO_BUSY_POLL};
use socket2::{Domain, Protocol, Socket, Type};
use std::{io, net::{SocketAddr, TcpStream}, os::fd::AsRawFd};

// at startup, never on the hot path
#[allow(clippy::unnecessary_cast)] // `4 as socklen_t`, as the chapter prints it
pub fn open_hot(addr: SocketAddr) -> io::Result<TcpStream> {
    let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    s.set_recv_buffer_size(4 << 20)?; // before connect: no autotuning
    s.set_send_buffer_size(1 << 20)?;
    s.connect(&addr.into())?;         // the round trip, paid here
    let s: TcpStream = s.into();
    s.set_nodelay(true)?;             // no Nagle
    let us: c_int = 50;               // busy-poll budget, µs
    let (p, n) = (&us as *const c_int, 4 as socklen_t);
    // SAFETY: p and n describe `us`, a live c_int, for the length of the call.
    let rc = unsafe {
        setsockopt(s.as_raw_fd(), SOL_SOCKET, SO_BUSY_POLL, p.cast(), n)
    };
    if rc != 0 { return Err(io::Error::last_os_error()); }
    s.set_nonblocking(true)?;         // WouldBlock, never sleep
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read};
    use std::net::TcpListener;

    #[test]
    fn the_hot_socket_is_set_up_or_refused_for_want_of_a_capability() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        match open_hot(l.local_addr().unwrap()) {
            Ok(mut s) => {
                assert!(s.nodelay().unwrap());
                let mut b = [0u8; 1];
                assert_eq!(s.read(&mut b).unwrap_err().kind(), ErrorKind::WouldBlock); // never sleeps
                let r = socket2::SockRef::from(&s);
                // 4 MB asked; the kernel caps it at net.core.rmem_max, then doubles it
                let rmem_max: usize = std::fs::read_to_string("/proc/sys/net/core/rmem_max")
                    .ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                assert!(r.recv_buffer_size().unwrap() >= (4usize << 20).min(2 * rmem_max));
            }
            Err(e) => assert_eq!(e.raw_os_error(), Some(libc::EPERM), "{e}"),
        }
    }
}

//! 4.10, how the sockets are set up: the hot socket opened once at startup,
//! buffers sized before connect and read back, Nagle off, SO_BUSY_POLL at 50 µs
//! with SO_PREFER_BUSY_POLL, non-blocking. Without CAP_NET_ADMIN the kernel
//! refuses SO_PREFER_BUSY_POLL (and, before 6.4, SO_BUSY_POLL above
//! net.core.busy_read), and `open_hot` returns that EPERM, as the chapter's code
//! does; a receive buffer capped at net.core.rmem_max is an error too.

use libc::{c_int, setsockopt, socklen_t, SOL_SOCKET, SO_BUSY_POLL, SO_PREFER_BUSY_POLL};
use socket2::{Domain, Protocol, Socket, Type};
use std::{io, net::{SocketAddr, TcpStream}, os::fd::AsRawFd};

// at startup, never on the hot path
#[allow(clippy::unnecessary_cast)] // `4 as socklen_t`, as the chapter prints it
pub fn open_hot(addr: SocketAddr) -> io::Result<TcpStream> {
    let s = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    s.set_recv_buffer_size(4 << 20)?; // before connect: no autotuning
    if s.recv_buffer_size()? < 8 << 20 { // read back doubled; less means capped at rmem_max
        return Err(io::Error::other("SO_RCVBUF capped: raise net.core.rmem_max to 4 MB"));
    }
    s.set_send_buffer_size(1 << 20)?;
    s.connect(&addr.into())?;         // the round trip, paid here
    let s: TcpStream = s.into();
    s.set_nodelay(true)?;             // no Nagle
    // busy-poll budget in µs; then the poller preferred to the interrupt (CAP_NET_ADMIN)
    for (opt, v) in [(SO_BUSY_POLL, 50 as c_int), (SO_PREFER_BUSY_POLL, 1)] {
        let (p, n) = (&v as *const c_int, 4 as socklen_t);
        // SAFETY: p and n describe `v`, a live c_int, for the length of the call.
        let rc = unsafe { setsockopt(s.as_raw_fd(), SOL_SOCKET, opt, p.cast(), n) };
        if rc != 0 { return Err(io::Error::last_os_error()); }
    }
    s.set_nonblocking(true)?;         // WouldBlock, never sleep
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{ErrorKind, Read};
    use std::net::TcpListener;

    #[test]
    fn the_hot_socket_is_set_up_or_refused_for_want_of_a_capability_or_buffer() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        match open_hot(l.local_addr().unwrap()) {
            Ok(mut s) => {
                assert!(s.nodelay().unwrap());
                let mut b = [0u8; 1];
                assert_eq!(s.read(&mut b).unwrap_err().kind(), ErrorKind::WouldBlock); // never sleeps
                // 4 MB asked, granted whole, read back doubled
                assert!(socket2::SockRef::from(&s).recv_buffer_size().unwrap() >= 8 << 20);
            }
            Err(e) => assert!(e.raw_os_error() == Some(libc::EPERM) || e.to_string().contains("rmem_max"),
                              "{e}"),
        }
    }
}

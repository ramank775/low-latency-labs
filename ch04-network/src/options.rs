//! 4.4, the options: Nagle off, quick acknowledgements, one message in one
//! call, and the two ways to hold partial segments that std does not offer.
//! The chapter's block as one function, the address and the message's two
//! pieces made arguments; it returns what `write_vectored` took.

use std::io::{IoSlice, Write};
use std::net::TcpStream;
use std::os::linux::net::TcpStreamExt; // set_quickack: Rust 1.89 or later
use socket2::SockRef;                  // socket2 0.6, feature "all"

/// Sends `hdr` + `body` three times: once gathered with `write_vectored`, once
/// under TCP_CORK, once with MSG_MORE on the header.
pub fn the_options(addr: &str, hdr: [u8; 16], body: [u8; 104]) -> std::io::Result<usize> {
    let mut s = TcpStream::connect(addr)?;
    s.set_nodelay(true)?;  // TCP_NODELAY: Nagle off for this socket
    s.set_quickack(true)?; // TCP_QUICKACK: until the kernel clears it

    // one message, one call, one segment; a short count must be finished
    let n = s.write_vectored(&[IoSlice::new(&hdr), IoSlice::new(&body)])?;
    if n < hdr.len() + body.len() {
        let rest: Vec<u8> = hdr.iter().chain(body.iter()).skip(n).copied().collect();
        s.write_all(&rest)?; // the short count, finished
    }

    // TCP_CORK and MSG_MORE are not in std
    let sock = SockRef::from(&s);
    sock.set_tcp_cork(true)?;  // hold partial segments...
    sock.send(&hdr)?;
    sock.send(&body)?;
    sock.set_tcp_cork(false)?; // ...and send them now, as one
    sock.send_with_flags(&hdr, libc::MSG_MORE)?; // hold this...
    sock.send(&body)?;                           // ...until this
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    #[test]
    fn all_three_ways_deliver_the_same_bytes() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let reader = std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut all = Vec::new();
            c.read_to_end(&mut all).unwrap();
            all
        });
        let mut hdr = [0u8; 16];
        hdr[0] = 120;
        let body = [7u8; 104];
        let n = the_options(&addr, hdr, body).expect("the options on loopback");
        assert_eq!(n, 120);
        let all = reader.join().unwrap();
        assert_eq!(all.len(), 3 * 120);
        for m in all.chunks(120) {
            assert_eq!(&m[..16], &hdr);
            assert_eq!(&m[16..], &body);
        }
    }
}

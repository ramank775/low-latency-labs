//! 4.1, what the program asks for: the two sides of a TCP conversation. The
//! chapter prints them as one block of statements; here each side is a
//! function, with the addresses made arguments so the pair runs on loopback.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

/// The side that waits: bind and listen, take one connection, read one
/// 48-byte message from it and answer with `reply`. Returns the caller's address.
pub fn the_side_that_waits(addr: &str, reply: &[u8]) -> std::io::Result<std::net::SocketAddr> {
    // the side that waits: socket, bind and listen in one call
    let listener = TcpListener::bind(addr)?;
    let (mut conn, peer) = listener.accept()?; // a new socket per connection
    let mut req = [0u8; 48];
    conn.read_exact(&mut req)?;
    conn.write_all(reply)?;
    Ok(peer)
}

/// The side that calls out: connect, write one order, and return whatever the
/// first read finds queued, which may be less than the far side wrote.
pub fn the_side_that_calls(addr: &str, order: [u8; 48]) -> std::io::Result<Vec<u8>> {
    let mut buf = [0u8; 1500];
    // the side that calls out: socket and connect, one round trip
    let mut s = TcpStream::connect(addr)?;
    s.write_all(&order)?;      // copied into the send queue, then returns
    let n = s.read(&mut buf)?; // whatever is queued: 1..=buf.len(), 0 at close
    Ok(buf[..n].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_two_sides_talk() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let addr = format!("127.0.0.1:{port}");
        let a = addr.clone();
        let waiter = std::thread::spawn(move || the_side_that_waits(&a, b"ack"));
        let order = crate::encode(1, 6_812_345, 512, 42, 0, 0);
        let mut got = None;
        for _ in 0..200 {
            // the waiting side may not have bound yet: try again shortly
            match the_side_that_calls(&addr, order) {
                Ok(v) => { got = Some(v); break; }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        let got = got.expect("connected within a second");
        assert!(!got.is_empty() && b"ack".starts_with(&got)); // 1..=3 bytes of the reply
        let peer = waiter.join().unwrap().expect("the waiting side");
        assert!(peer.ip().is_loopback());
    }
}

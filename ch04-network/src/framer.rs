//! 4.7, a stream is not a message: one buffer and two cursors. Each read
//! appends at `end`; each complete message is handed out as a slice where it
//! lies; only a partial tail, less than one message, is ever moved.

use std::io::{self, ErrorKind, Read};
use std::net::TcpStream;

pub const MAX_MSG: usize = 1024;

pub struct Framer {
    buf: Box<[u8]>, // one buffer, allocated once at startup
    start: usize,   // first byte not yet handed to the parser
    end: usize,     // one past the last byte received
}

impl Framer {
    /// At startup: the one allocation, `capacity` bytes (the chapter's 64 KB).
    pub fn new(capacity: usize) -> Framer {
        assert!(capacity >= 2 * MAX_MSG, "the buffer must hold a partial tail and a read");
        Framer { buf: vec![0u8; capacity].into_boxed_slice(), start: 0, end: 0 }
    }

    /// One read from the socket, then every complete message to `on_msg`.
    #[allow(clippy::manual_range_contains)] // `len < 2 || len > MAX_MSG`, as the chapter prints it
    pub fn poll(&mut self, sock: &mut TcpStream, mut on_msg: impl FnMut(&[u8]))
        -> io::Result<()>
    {
        if self.buf.len() - self.end < MAX_MSG {
            // less than a message of room: move the partial tail to the front
            self.buf.copy_within(self.start..self.end, 0);
            self.end -= self.start;
            self.start = 0;
        }
        match sock.read(&mut self.buf[self.end..]) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()), // closed
            Ok(n) => self.end += n,
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(()),
            Err(e) => return Err(e),
        }
        // each message starts with its length: a little-endian u16 that
        // counts its own two bytes
        while self.end - self.start >= 2 {
            let at = self.start;
            let len = u16::from_le_bytes([self.buf[at], self.buf[at + 1]]);
            let len = len as usize;
            if len < 2 || len > MAX_MSG {
                return Err(ErrorKind::InvalidData.into());
            }
            if self.end - at < len {
                break; // the rest has not arrived yet: leave it where it is
            }
            on_msg(&self.buf[at..at + len]);
            self.start = at + len;
        }
        if self.start == self.end {
            (self.start, self.end) = (0, 0);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;

    fn pair() -> (TcpStream, TcpStream) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (s, _) = l.accept().unwrap();
        (c, s)
    }

    #[test]
    fn messages_split_across_reads_come_out_whole_and_in_order() {
        let (mut tx, mut rx) = pair();
        tx.set_nodelay(true).unwrap();
        let w = std::thread::spawn(move || {
            // 10,000 updates written in pieces of 7, 31 and 113 bytes, so reads end mid-message
            let mut all = Vec::new();
            for s in 0..10_000u64 {
                all.extend_from_slice(&crate::encode(s, 6_812_345 + s as i64, 512, 42, (s & 1) as u8, s));
            }
            let mut at = 0;
            for (k, n) in [7usize, 31, 113].iter().cycle().enumerate() {
                if at >= all.len() { break; }
                let e = (at + n).min(all.len());
                tx.write_all(&all[at..e]).unwrap();
                at = e;
                if k % 64 == 0 { std::thread::yield_now(); }
            }
        });
        let mut f = Framer::new(64 << 10);
        let mut next = 0u64;
        while next < 10_000 {
            f.poll(&mut rx, |m| {
                let u = crate::parse(m).expect("a whole depth update");
                assert_eq!(u.seq.get(), next);
                next += 1;
            }).expect("poll");
        }
        w.join().unwrap();
    }

    #[test]
    fn an_impossible_length_is_an_error_not_a_wait() {
        let (mut tx, mut rx) = pair();
        tx.write_all(&[0xff, 0xff, 0, 0]).unwrap(); // 65,535: more than MAX_MSG
        let mut f = Framer::new(4096);
        let e = f.poll(&mut rx, |_| panic!("no message")).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::InvalidData);
    }

    #[test]
    fn a_closed_stream_is_unexpected_eof() {
        let (tx, mut rx) = pair();
        drop(tx);
        let mut f = Framer::new(4096);
        assert_eq!(f.poll(&mut rx, |_| {}).unwrap_err().kind(), ErrorKind::UnexpectedEof);
    }
}

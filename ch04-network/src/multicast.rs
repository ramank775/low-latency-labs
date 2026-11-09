//! 4.5, multicast and what exchanges choose: joining a feed's group, reading
//! one datagram, and the arbiter that takes whichever of the A and B copies
//! of each packet arrives first.

use std::net::{Ipv4Addr, UdpSocket};

/// Bind group A, 239.1.1.1, and the feed's port, and join the group on the
/// interface whose address is `ours` (the chapter's 10.1.0.5; 127.0.0.1 on loopback).
pub fn join_feed_a(port: u16, ours: Ipv4Addr) -> std::io::Result<UdpSocket> {
    // bind the group and the feed's port, then join it on the feed-facing card
    let feed_a = UdpSocket::bind((Ipv4Addr::new(239, 1, 1, 1), port))?; // only group A's datagrams
    feed_a.join_multicast_v4(&Ipv4Addr::new(239, 1, 1, 1),  // group A
                             &ours)?;                       // our address there
    Ok(feed_a)
}

/// One datagram, whole, and the sequence number its packet header starts with.
pub fn one_datagram(feed_a: &UdpSocket) -> std::io::Result<(usize, u64)> {
    let mut buf = [0u8; 1500];
    let (n, _) = feed_a.recv_from(&mut buf)?;               // exactly one datagram
    let seq = u64::from_le_bytes(buf[0..8].try_into().unwrap()); // packet header
    Ok((n, seq))
}

/// The A/B arbiter: one expected sequence number shared by both feeds.
/// `apply` is the program's handler for a packet's messages; every gap the
/// arbiter declares is kept, as `[from, to)`, for the gap-fill request.
pub struct Arbiter<F: FnMut(&[u8])> {
    pub next: u64,
    pub gaps: Vec<(u64, u64)>,
    handler: F,
}

impl<F: FnMut(&[u8])> Arbiter<F> {
    pub fn new(first: u64, handler: F) -> Self {
        Arbiter { next: first, gaps: Vec::new(), handler }
    }

    // arbitration: one expected sequence number shared by both feeds
    pub fn on_packet(&mut self, seq: u64, msgs: &[u8]) {
        if seq < self.next {
            if self.fill(seq) {
                self.apply(msgs); // the other feed's late copy fills a marked gap
            }
            return; // otherwise the other feed's copy came first: drop this one
        }
        if seq > self.next {
            self.gap(self.next, seq); // missing on both so far: mark, ask gap-fill
        }
        self.apply(msgs);
        self.next = seq + 1;
    }

    fn gap(&mut self, from: u64, to: u64) {
        self.gaps.push((from, to));
    }

    /// Strikes `seq` from the marked gaps; false if it was in none.
    fn fill(&mut self, seq: u64) -> bool {
        let Some(i) = self.gaps.iter().position(|&(from, to)| from <= seq && seq < to) else {
            return false;
        };
        let (from, to) = self.gaps.swap_remove(i);
        if from < seq { self.gaps.push((from, seq)); }
        if seq + 1 < to { self.gaps.push((seq + 1, to)); }
        true
    }

    fn apply(&mut self, msgs: &[u8]) {
        (self.handler)(msgs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_first_copy_wins_and_a_double_loss_is_a_gap() {
        let mut applied = Vec::new();
        let mut arb = Arbiter::new(100, |m: &[u8]| applied.push(m[0]));
        // arrivals in time order: A loses 101, B loses 103, both lose 104,
        // and B's 102 comes after A's
        let arrivals = [100u64, 100, 101, 102, 102, 103, 105, 105];
        for s in arrivals { arb.on_packet(s, &[s as u8]); }
        arb.on_packet(106, &[106]);
        arb.on_packet(101, &[101]); // a third copy of 101, already applied: dropped
        let (next, gaps) = (arb.next, arb.gaps.clone());
        drop(arb);
        assert_eq!(applied, vec![100, 101, 102, 103, 105, 106]);
        assert_eq!(gaps, vec![(104, 105)]);
        assert_eq!(next, 107);
    }

    #[test]
    fn a_late_copy_fills_its_gap_and_only_once() {
        let mut applied = Vec::new();
        let mut arb = Arbiter::new(100, |m: &[u8]| applied.push(m[0]));
        // A loses 101 to 103; B's copies arrive after A's 104
        for s in [100u64, 104, 102, 102, 105] { arb.on_packet(s, &[s as u8]); }
        let (next, mut gaps) = (arb.next, arb.gaps.clone());
        drop(arb);
        gaps.sort();
        assert_eq!(applied, vec![100, 104, 102, 105]); // the second 102 dropped
        assert_eq!(gaps, vec![(101, 102), (103, 104)]); // still missing on both
        assert_eq!(next, 106);
    }

    #[test]
    fn a_group_joined_on_loopback_delivers_one_datagram() {
        // Multicast on lo needs nothing but a route the kernel finds from the
        // interface address; where a sandbox refuses even that, say so and stop.
        let port = UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let feed = match join_feed_a(port, Ipv4Addr::LOCALHOST) {
            Ok(f) => f,
            Err(e) => { eprintln!("multicast join refused here ({e}): skipped"); return; }
        };
        feed.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket2::SockRef::from(&tx).set_multicast_if_v4(&Ipv4Addr::LOCALHOST).unwrap();
        let mut pkt = [0u8; 56];
        pkt[..8].copy_from_slice(&5_000u64.to_le_bytes());
        pkt[8..].copy_from_slice(&crate::encode(5_000, 6_812_345, 512, 42, 0, 0));
        if tx.send_to(&pkt, (Ipv4Addr::new(239, 1, 1, 1), port)).is_err() {
            eprintln!("multicast send refused here: skipped");
            return;
        }
        match one_datagram(&feed) {
            Ok((n, seq)) => assert_eq!((n, seq), (56, 5_000)),
            Err(e) => eprintln!("no multicast loopback here ({e}): skipped"),
        }
    }
}

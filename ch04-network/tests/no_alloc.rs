// tests/no_alloc.rs, a binary of its own: the counters see every thread
use ch04_network::counting::{Counting, ALLOCS};
use ch04_network::{encode, Framer};
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::Ordering::Relaxed;

#[global_allocator]
static GLOBAL: Counting = Counting;

fn on_update(m: &[u8]) {
    let u = ch04_network::parse(m).expect("a depth update");
    std::hint::black_box((u.seq.get(), u.price.get(), u.qty.get()));
}

/// 4.7's check, the chapter's lines in the middle: a million depth updates
/// read off a TCP connection through the framer and the parser, and not one
/// allocation after warm-up.
#[test]
fn the_hot_path_allocates_nothing() -> std::io::Result<()> {
    assert!(ch04_network::counting::installed());
    let l = TcpListener::bind("127.0.0.1:0")?;
    let mut sock = TcpStream::connect(l.local_addr()?)?;
    let (mut venue, _) = l.accept()?;
    // the venue: 1,020,000 updates, 1,000 to a write, from one buffer built before it starts
    let mut batch = Vec::with_capacity(48 * 1_000);
    for s in 0..1_000u64 {
        batch.extend_from_slice(&encode(s, 6_812_345 + (s % 7) as i64, 512, 42, (s & 1) as u8, s));
    }
    let feed = std::thread::spawn(move || -> std::io::Result<()> {
        for _ in 0..1_020 { venue.write_all(&batch)?; }
        Ok(())
    });
    let mut framer = Framer::new(64 << 10);
    let mut warm = 0u64;
    while warm < 10_000 {
        framer.poll(&mut sock, |m| { on_update(m); warm += 1; })?;
    }

    let before = ALLOCS.load(Relaxed); // after warm-up: connected, subscribed
    let mut seen = 0u64;
    while seen < 1_000_000 {
        framer.poll(&mut sock, |m| { on_update(m); seen += 1; })?;
    }
    assert_eq!(ALLOCS.load(Relaxed), before, "the hot path allocated");

    drop(sock); // the venue's last writes may now fail: that is the end of the feed
    let _ = feed.join();
    Ok(())
}

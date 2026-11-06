// tests/no_alloc.rs, a binary of its own
mod common;                    // the counting allocator, plus FREES
use ch05_structures::{ring, Pool, Side, Update};
use common::{ALLOCS, FREES};
use std::sync::atomic::Ordering::Relaxed;

fn update(seq: u64) -> Update {
    Update { seq, price: 6_812_345 + (seq % 7) as i64, qty: (seq % 500) as i64, instrument: 42, side: Side::Bid }
}

/// One test per binary: the counters see every thread, so nothing else may run beside it.
#[test]
fn ring_and_pool_allocate_nothing() {
    assert!(ch05_structures::counting::installed());
    let (mut tx, mut rx) = ring::<Update, 4096>();             // startup allocates here
    let mut pool: Pool<Update> = Pool::with_capacity(4_096);
    let mut sum = 0u64;
    for s in 0..10_000 { let _ = tx.try_push(update(s)); rx.drain(|u| sum += u.seq); } // warm-up

    let (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
    for s in 0..1_000_000u64 {
        assert!(tx.try_push(update(s)).is_ok());
        if s % 64 == 63 { rx.drain(|u| sum += u.seq); }        // a batch, published once
        let i = pool.alloc(update(s)).expect("pool exhausted");
        sum += pool.get(i).qty as u64;
        pool.free(i);
    }
    rx.drain(|u| sum += u.seq);
    let (a1, f1) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));

    assert_eq!((a1 - a0, f1 - f0), (0, 0), "allocated in the loop");
    assert!(sum > 0);
}

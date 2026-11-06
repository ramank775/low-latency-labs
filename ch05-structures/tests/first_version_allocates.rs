// tests/first_version_allocates.rs, a binary of its own: 5.1's first `Update`
// costs two allocations and two frees a message, and the fixed one none.
mod common;                    // the counting allocator, plus FREES
use ch05_structures::{first_version, Side, Update};
use common::{ALLOCS, FREES};
use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;

#[test]
fn the_first_update_allocates_twice_a_message_and_the_fixed_one_never() {
    assert!(ch05_structures::counting::installed());
    let n = 10_000u64;

    let (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
    for seq in 0..n {
        let u = first_version::Update::decode("BTC-USDT-PERP", seq % 2 == 0, 68_123.45, 0.512, seq);
        black_box(&u);
    }                                                    // dropped: the two frees
    let (a1, f1) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
    assert_eq!((a1 - a0, f1 - f0), (2 * n, 2 * n), "two allocations and two frees a message");

    for seq in 0..n {
        let u = Update { seq, price: 6_812_345, qty: 512, instrument: 42, side: Side::Bid };
        black_box(&u);
    }
    assert_eq!((ALLOCS.load(Relaxed), FREES.load(Relaxed)), (a1, f1), "the fixed Update allocated");
}

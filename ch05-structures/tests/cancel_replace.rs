// tests/cancel_replace.rs, a binary of its own: 5.3's pooled cancel-replace.
mod common;                    // the counting allocator, plus FREES
use ch05_structures::Pool;
use common::{ALLOCS, FREES};
use std::sync::atomic::Ordering::Relaxed;

/// The gateway's 64-byte order, a line per slot.
#[repr(align(64))]
struct Order { id: u64, _price: i64, _qty: i64 }

impl Order {
    fn new(n: u64) -> Order { Order { id: n, _price: 6_812_345, _qty: 500 } }
}

#[test]
fn cancel_replace_allocates_nothing() {
    assert_eq!(size_of::<Order>(), 64);
    let mut pool: Pool<Order> = Pool::with_capacity(4_096); // startup allocates here
    let before = ALLOCS.load(Relaxed);
    let frees = FREES.load(Relaxed);
    let mut last = 0;
    for n in 0..1_000_000u64 {
        let i = pool.alloc(Order::new(n)).expect("pool exhausted");
        last = pool.get(i).id;
        pool.free(i);
    }
    assert_eq!(ALLOCS.load(Relaxed), before); // the loop allocated nothing
    assert_eq!(FREES.load(Relaxed), frees);   // nor freed anything
    assert_eq!(last, 999_999);
}

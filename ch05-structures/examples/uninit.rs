// 5.8's uninitialised slots: `cargo run --release -p ch05-structures --example uninit`.
// The commented line compiles too, which is the danger; `cargo +nightly miri test` catches it.
use std::mem::MaybeUninit;
use ch05_structures::{Side, Update}; // the 32-byte update

fn main() {
    // A ring's or a pool's 4,096 slots, 128 KB: allocated, never zeroed.
    let mut slots: Vec<MaybeUninit<Update>> = Vec::with_capacity(4096);
    slots.resize_with(4096, MaybeUninit::uninit); // writes nothing
    // The producer's push: writing is always allowed.
    slots[7].write(Update { seq: 1_873_465_902, price: 6_812_345, qty: 512, instrument: 42, side: Side::Bid });
    // The consumer's pop: reading is allowed only after a write.
    // SAFETY: slot 7 was written on the line above.
    let u: Update = unsafe { slots[7].assume_init_read() };
    // let bad = unsafe { slots[8].assume_init_read() }; // slot 8 never written: UB
    println!("{} {} {}", u.seq, u.price, u.qty);
}

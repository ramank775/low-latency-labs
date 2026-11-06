//! Section 3.5's worked example: the quote, published properly. One pair,
//! `Release` on the strategy's last store and `Acquire` on the gateway's spin.

use std::sync::atomic::{AtomicU64, Ordering::{Acquire, Relaxed, Release}};

pub struct QuoteCell {
    px: AtomicU64,
    qty: AtomicU64,
    seq: AtomicU64,
}

impl QuoteCell {
    // strategy thread, the only writer
    pub fn publish(&self, seq: u64, px: u64, qty: u64) {
        self.px.store(px, Relaxed);
        self.qty.store(qty, Relaxed);
        self.seq.store(seq, Release); // px and qty are visible no later than seq
    }

    // gateway thread
    pub fn wait_for(&self, seq: u64) -> (u64, u64) {
        while self.seq.load(Acquire) < seq {
            std::hint::spin_loop();
        }
        (self.px.load(Relaxed), self.qty.load(Relaxed)) // at least as new as seq
    }
}

impl QuoteCell {
    /// Nothing published yet: sequence 0.
    pub fn new() -> Self {
        QuoteCell { px: AtomicU64::new(0), qty: AtomicU64::new(0), seq: AtomicU64::new(0) }
    }
}

impl Default for QuoteCell {
    fn default() -> Self { Self::new() }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn the_gateway_sees_the_quote_or_a_later_one() {
        let q = QuoteCell::new();
        const N: u64 = 100_000;
        std::thread::scope(|s| {
            s.spawn(|| for seq in 1..=N { q.publish(seq, 10_000 + seq, seq) });
            s.spawn(|| for seq in (1..=N).step_by(97) {
                let (px, qty) = q.wait_for(seq);
                // "at least as new as seq": never the previous quote's price or quantity
                assert!(px >= 10_000 + seq && qty >= seq, "seq {seq}: px {px} qty {qty}");
            });
        });
    }
}

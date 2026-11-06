//! 5.3's arena: one block, an offset into it, and a reset at the end of each
//! batch. `Intent`, an order the strategy has decided to send, is the glue the
//! chapter's listing names and does not show.

use crate::{Side, Update};
use bumpalo::Bump;

/// An order the strategy has decided to send; it lives until the batch ends.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Intent { pub instrument: u32, pub side: Side, pub price: i64, pub qty: i64 }

impl From<&Update> for Intent {
    /// The toy decision: lean on the price just updated.
    fn from(u: &Update) -> Intent { Intent { instrument: u.instrument, side: u.side, price: u.price, qty: u.qty } }
}

pub fn run(batches: &[Vec<Update>], mut send: impl FnMut(&[Intent])) {
    let mut arena = Bump::with_capacity(64 * 1024); // one 64 KB block, at startup
    for batch in batches {
        // Values whose lives end with the batch: each allocation bumps the offset.
        let intents = arena.alloc_slice_fill_iter(batch.iter().map(Intent::from));
        send(intents);
        arena.reset(); // all gone at once; the block is kept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_batch_sees_its_own_intents() {
        let u = |seq| Update { seq, price: 6_812_345, qty: 512, instrument: 42, side: Side::Bid };
        let batches = vec![vec![u(1), u(2)], vec![u(3)]];
        let mut seen = Vec::new();
        run(&batches, |i| seen.push(i.len()));
        assert_eq!(seen, [2, 1]);
    }
}

//! 5.1's first `Update`, kept to be measured against the fixed one: two
//! strings, so two allocations on the feed's core and two frees on the
//! strategy's for every message. `tests/first_version_allocates.rs` counts them.

pub struct Update {            // first version
    pub symbol: String,        // "BTC-USDT-PERP": 24 bytes here, 13 on the heap
    pub side: String,          // "bid" or "ask": 24 bytes here, 3 on the heap
    pub price: f64,            // dollars
    pub qty: f64,              // bitcoin
    pub seq: u64,
}

impl Update {
    /// What a first decoder builds per message: the symbol's `String::from`
    /// and the side's `to_string`.
    pub fn decode(symbol: &str, bid: bool, price: f64, qty: f64, seq: u64) -> Update {
        Update { symbol: String::from(symbol), side: (if bid { "bid" } else { "ask" }).to_string(), price, qty, seq }
    }
}

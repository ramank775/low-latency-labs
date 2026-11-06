//! The synthetic feed `gen` writes: one instrument's depth updates in the
//! book chapter's shape, about 95% on a deep level, 4% on the best level's
//! quantity and 1% moving the best price, with consecutive sequence numbers,
//! Poisson-like arrivals at a chosen average rate, and bursts of up to 30
//! messages a few nanoseconds apart when the price moves.
//!
//! The generator keeps its own copy of the venue's book, so every update it
//! writes is one a real venue could send: the book is never crossed, neither
//! side is ever empty after the first two messages, and every price stays
//! well inside the dense array's 65,536-level window. Deterministic from the seed.

use std::collections::BTreeMap;
use std::io::{self, Write};

pub struct Params {
    pub records: u64,       // messages to write
    pub rate: f64,          // average messages per second, bursts included
    pub seed: u64,
    pub base_price: i64,    // the opening best bid, in ticks: 6,812,345 is $68,123.45
    pub depth: i64,         // levels kept on each side of the book
    pub instrument: u32,    // 42 of 64
}

impl Default for Params {
    fn default() -> Self {
        Params { records: 2_000_000, rate: 100_000.0, seed: 1, base_price: 6_812_345, depth: 100, instrument: 42 }
    }
}

/// What was written, for gen to print.
#[derive(Default, Debug)]
pub struct Written {
    pub records: u64,
    pub deep: u64,
    pub best_qty: u64,
    pub best_moves: u64,      // updates that changed the best price on their side
    pub bursts: u64,
    pub last_arrival_ns: u64,
    pub min_price: i64,
    pub max_price: i64,
}

/// splitmix64: small, fast, and the same numbers on every machine.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn unit(&mut self) -> f64 { (self.next() >> 11) as f64 / (1u64 << 53) as f64 }
    fn range(&mut self, lo: i64, hi: i64) -> i64 { lo + (self.next() % (hi - lo + 1) as u64) as i64 }
    fn qty(&mut self) -> i64 { self.range(1, 2_000) }
}

const BURST_P: f64 = 0.0025;      // chance an event is a price move, a burst
const BURST_MAX: i64 = 30;        // messages in the longest burst
const BURST_MEAN: f64 = 20.0;     // messages in the average burst: uniform from 4k + 2 to 30
const BEST_QTY_P: f64 = 0.04;     // chance a single message changes the best quantity
const REMOVE_P: f64 = 0.10;       // chance a deep update removes the level it names

struct Gen<'w, W: Write> {
    out: &'w mut W,
    p: &'w Params,
    rng: Rng,
    bids: BTreeMap<i64, i64>,
    asks: BTreeMap<i64, i64>,
    seq: u64,
    t: f64,                       // ns since the first record
    w: Written,
}

impl<W: Write> Gen<'_, W> {
    fn full(&self) -> bool { self.w.records >= self.p.records }
    fn best_bid(&self) -> i64 { *self.bids.keys().next_back().expect("bids never empty") }
    fn best_ask(&self) -> i64 { *self.asks.keys().next().expect("asks never empty") }

    /// Write one update and apply it to the generator's own book.
    fn emit(&mut self, bid: bool, price: i64, qty: i64) -> io::Result<()> {
        if self.full() { return Ok(()); }
        let side_map = if bid { &mut self.bids } else { &mut self.asks };
        if qty == 0 { side_map.remove(&price); } else { side_map.insert(price, qty); }
        self.seq += 1;
        let t_ns = self.t.round() as u64;
        let venue_ts = 1_761_000_000_000_000_000 + t_ns;          // the venue's clock: any epoch
        let msg = ch04_network::encode(self.seq, price, qty, self.p.instrument, if bid { 0 } else { 1 }, venue_ts);
        self.out.write_all(&t_ns.to_le_bytes())?;
        self.out.write_all(&msg)?;
        self.w.records += 1;
        self.w.last_arrival_ns = t_ns;
        self.w.min_price = self.w.min_price.min(price);
        self.w.max_price = self.w.max_price.max(price);
        Ok(())
    }

    /// The gap before the next event: exponential, so arrivals are a Poisson stream.
    fn gap(&mut self) {
        let per_event = 1.0 + BURST_P * (BURST_MEAN - 1.0);   // messages per event, bursts included
        let mean = 1e9 / self.p.rate * per_event;
        self.t += -mean * (1.0 - self.rng.unit()).ln();
    }

    /// Inside a burst: a few nanoseconds apart.
    fn tick(&mut self) { self.t += self.rng.range(1, 10) as f64; }

    fn deep(&mut self) -> io::Result<()> {
        let bid = self.rng.next() & 1 == 0;
        let d = self.rng.range(1, self.p.depth);
        let price = if bid { self.best_bid() - d } else { self.best_ask() + d };
        let exists = if bid { self.bids.contains_key(&price) } else { self.asks.contains_key(&price) };
        let qty = if exists && self.rng.unit() < REMOVE_P { 0 } else { self.rng.qty() };
        self.w.deep += 1;
        self.emit(bid, price, qty)
    }

    fn best_qty(&mut self) -> io::Result<()> {
        let bid = self.rng.next() & 1 == 0;
        let price = if bid { self.best_bid() } else { self.best_ask() };
        self.w.best_qty += 1;
        let q = self.rng.qty();
        self.emit(bid, price, q)
    }

    /// The price moves k ticks: the levels it moves through go first, so the
    /// book is never crossed, then the other side follows, then the far ends
    /// are trimmed and extended to keep the depth, then deep refreshes fill the burst.
    fn burst(&mut self) -> io::Result<()> {
        self.w.bursts += 1;
        let k = self.rng.range(1, 3);
        let mid_drift = (self.best_bid() - self.p.base_price) as f64 / 2_000.0;
        let up = self.rng.unit() < (0.5 - mid_drift.clamp(-0.4, 0.4));   // mean-reverting walk
        let len = self.rng.range(4 * k + 2, BURST_MAX);
        let mut sent = 0;
        let (bb, ba, depth) = (self.best_bid(), self.best_ask(), self.p.depth);
        if up {
            let nb = bb + k;                                            // the new best bid
            let gone: Vec<i64> = self.asks.range(..=nb).map(|(&p, _)| p).collect();
            for p in gone { self.tick(); self.emit(false, p, 0)?; self.w.best_moves += 1; sent += 1; }
            if !self.asks.contains_key(&(nb + 1)) {                    // keep the spread one tick
                self.tick(); let q = self.rng.qty(); self.emit(false, nb + 1, q)?; self.w.best_moves += 1; sent += 1;
            }
            for p in bb + 1..=nb { self.tick(); let q = self.rng.qty(); self.emit(true, p, q)?; self.w.best_moves += 1; sent += 1; }
            let far: Vec<i64> = self.bids.range(..nb - depth).map(|(&p, _)| p).collect();
            for p in far { self.tick(); self.emit(true, p, 0)?; sent += 1; }
            let top = self.best_ask() + depth;
            for p in top - k + 1..=top {
                if !self.asks.contains_key(&p) { self.tick(); let q = self.rng.qty(); self.emit(false, p, q)?; sent += 1; }
            }
        } else {
            let na = ba - k;                                            // the new best ask
            let gone: Vec<i64> = self.bids.range(na..).map(|(&p, _)| p).collect();
            for p in gone { self.tick(); self.emit(true, p, 0)?; self.w.best_moves += 1; sent += 1; }
            if !self.bids.contains_key(&(na - 1)) {                    // keep the spread one tick
                self.tick(); let q = self.rng.qty(); self.emit(true, na - 1, q)?; self.w.best_moves += 1; sent += 1;
            }
            for p in na..ba { self.tick(); let q = self.rng.qty(); self.emit(false, p, q)?; self.w.best_moves += 1; sent += 1; }
            let far: Vec<i64> = self.asks.range(na + depth + 1..).map(|(&p, _)| p).collect();
            for p in far { self.tick(); self.emit(false, p, 0)?; sent += 1; }
            let bottom = self.best_bid() - depth;
            for p in bottom..bottom + k {
                if !self.bids.contains_key(&p) { self.tick(); let q = self.rng.qty(); self.emit(true, p, q)?; sent += 1; }
            }
        }
        while sent < len && !self.full() { self.tick(); self.deep()?; sent += 1; }
        Ok(())
    }
}

/// Write `p.records` records of 56 bytes to `out`.
pub fn write(p: &Params, out: &mut impl Write) -> io::Result<Written> {
    let mut g = Gen { out, p, rng: Rng(p.seed), bids: BTreeMap::new(), asks: BTreeMap::new(),
                      seq: 0, t: 0.0, w: Written { min_price: i64::MAX, max_price: i64::MIN, ..Default::default() } };
    // the opening book, inside out and alternating, so both sides exist from the second message
    for i in 0..p.depth {
        if g.full() { break; }
        let q = g.rng.qty();
        g.emit(true, p.base_price - i, q)?;
        g.gap();
        let q = g.rng.qty();
        g.emit(false, p.base_price + 1 + i, q)?;
        g.gap();
    }
    while !g.full() {
        let u = g.rng.unit();
        if u < BURST_P { g.burst()?; }
        else if u < BURST_P + BEST_QTY_P { g.best_qty()?; }
        else { g.deep()?; }
        g.gap();
    }
    Ok(g.w)
}

/// The same file, in memory: for tests.
pub fn generate(p: &Params) -> Vec<u8> {
    let mut v = Vec::with_capacity(p.records as usize * crate::RECORD);
    write(p, &mut v).expect("writing to a Vec cannot fail");
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, Replay};
    use ch06_book::{crossed, Book, TreeBook};

    #[test]
    fn the_feed_is_in_order_never_crossed_and_never_one_sided() {
        let p = Params { records: 200_000, ..Default::default() };
        let r = Replay::from_bytes(generate(&p));
        assert_eq!(r.len(), 200_000);
        let mut book = TreeBook::default();
        let (mut last_t, mut seq) = (0, 0);
        for (k, rec) in r.records().enumerate() {
            let u = decode(rec.msg).expect("a depth update");
            assert_eq!(u.seq, seq + 1);
            seq = u.seq;
            assert!(rec.t_arrival_ns >= last_t);
            last_t = rec.t_arrival_ns;
            book.apply(&u);
            assert!(!crossed(&book), "crossed at seq {}", u.seq);
            if k >= 1 { assert!(book.best_bid().is_some() && book.best_ask().is_some()); }
            assert!((u.price - p.base_price).abs() < 30_000, "outside the window");
        }
        let secs = last_t as f64 / 1e9;
        assert!((1.6..2.4).contains(&secs), "200,000 at 100,000/s should take about 2 s, took {secs}");
    }

    #[test]
    fn the_same_seed_writes_the_same_bytes() {
        let p = Params { records: 20_000, seed: 9, ..Default::default() };
        assert_eq!(generate(&p), generate(&p));
        assert_ne!(generate(&p), generate(&Params { seed: 10, ..p }));
    }
}

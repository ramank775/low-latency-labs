//! The generator of updates: one instrument's depth updates in the shape the
//! replay lab asks for, deterministic from a seed. After an opening that
//! fills the book, about 95% of updates land on a level deep in the book, 4%
//! change the best level's quantity, and 1% move the best price, with short
//! walks: the best emptied, so the cursor walks a few levels in, or a new
//! level inside the spread.
//!
//! The generator keeps its own copy of the venue's book, so every update is
//! one a real venue could send: never crossed, neither side ever empty after
//! the opening, consecutive sequence numbers from 1, quantities absolute and 0
//! for a removed level. The capstone's `gen` writes the same shape with bursts
//! and arrival times; this one has neither, and the book chapter cannot depend
//! on the capstone, which depends on it.
//!
//! Also here: the property test's adversarial stream, and the 56-byte replay
//! record (an arrival time, then the networking chapter's 48-byte message),
//! read and written without depending on the networking crate.

use crate::{Lots, Side, Ticks, Update};
use ch05_structures::lab::Rng;
use std::collections::BTreeMap;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

pub struct Params {
    pub records: u64,       // updates to make, the opening included
    pub seed: u64,
    pub best_bid: Ticks,    // the opening best bid: 6,812,345 is $68,123.45
    pub spread: Ticks,      // the opening spread, in ticks
    pub depth: Ticks,       // deep updates land 1 to `depth` levels behind the best
    pub best_qty: f64,      // share of updates on the best level's quantity: 0.04
    pub moves: f64,         // share that move the best price: 0.01
    pub trend: bool,        // every move upward: the price leaves the window (rebase)
    pub instrument: u32,    // 42
}

impl Default for Params {
    fn default() -> Self {
        Params { records: 10_000_000, seed: 7, best_bid: 6_812_345, spread: 3, depth: 1_000,
                 best_qty: 0.04, moves: 0.01, trend: false, instrument: 42 }
    }
}

/// What was made, for a lab to print.
#[derive(Default, Debug, Clone, Copy)]
pub struct Made {
    pub opening: u64,
    pub deep: u64,
    pub best_qty: u64,
    pub moves: u64,         // updates that moved the best price on their side
    pub min_price: Ticks,
    pub max_price: Ticks,
}

const REMOVE: f64 = 0.10;   // chance a deep update on an existing level removes it

struct Gen<'p> {
    p: &'p Params,
    rng: Rng,
    bids: BTreeMap<Ticks, Lots>,
    asks: BTreeMap<Ticks, Lots>,
    out: Vec<Update>,
    made: Made,
    first_mid2: Ticks,      // twice the opening mid
}

impl Gen<'_> {
    fn full(&self) -> bool { self.out.len() as u64 >= self.p.records }
    fn best_bid(&self) -> Ticks { *self.bids.keys().next_back().expect("bids never empty") }
    fn best_ask(&self) -> Ticks { *self.asks.keys().next().expect("asks never empty") }
    fn qty(&mut self) -> Lots { 1 + self.rng.below(2_000) as Lots }

    fn emit(&mut self, side: Side, price: Ticks, qty: Lots) {
        if self.full() { return; }
        let book = match side { Side::Bid => &mut self.bids, Side::Ask => &mut self.asks };
        if qty == 0 { book.remove(&price); } else { book.insert(price, qty); }
        let seq = self.out.len() as u64 + 1;
        self.out.push(Update { seq, price, qty, instrument: self.p.instrument, side });
        self.made.min_price = self.made.min_price.min(price);
        self.made.max_price = self.made.max_price.max(price);
    }

    fn side(&mut self) -> Side { if self.rng.next_u64() & 1 == 0 { Side::Bid } else { Side::Ask } }

    /// 1 to depth levels behind the best: overwrite, create, or now and then remove;
    /// a side grown far past its depth loses its farthest level instead.
    fn deep(&mut self) {
        self.made.deep += 1;
        let side = self.side();
        let book = match side { Side::Bid => &self.bids, Side::Ask => &self.asks };
        if book.len() as Ticks > self.p.depth + self.p.depth / 4 {
            let far = match side { Side::Bid => *book.keys().next().unwrap_or(&0), Side::Ask => *book.keys().next_back().unwrap_or(&0) };
            return self.emit(side, far, 0);                       // trimmed, so the book stays near its depth
        }
        let d = 1 + self.rng.below(self.p.depth as u64) as Ticks;
        let price = match side { Side::Bid => self.best_bid() - d, Side::Ask => self.best_ask() + d };
        let exists = match side { Side::Bid => self.bids.contains_key(&price), Side::Ask => self.asks.contains_key(&price) };
        let qty = if exists && self.rng.unit() < REMOVE { 0 } else { self.qty() };
        self.emit(side, price, qty);
    }

    fn best_qty(&mut self) {
        self.made.best_qty += 1;
        let side = self.side();
        let (price, old) = match side {
            Side::Bid => { let p = self.best_bid(); (p, self.bids[&p]) }
            Side::Ask => { let p = self.best_ask(); (p, self.asks[&p]) }
        };
        let mut q = self.qty();
        if q == old { q = old % 2_000 + 1; }                     // a change, never a repeat
        self.emit(side, price, q);
    }

    /// The best price moves one way: up is the best ask emptied or a bid inside
    /// the spread; down is the best bid emptied or an ask inside the spread.
    /// Mean-reverting about the opening mid, unless `trend`.
    fn best_move(&mut self) {
        self.made.moves += 1;
        let (bb, ba) = (self.best_bid(), self.best_ask());
        let drift = (bb + ba - self.first_mid2) as f64 / 4_000.0;
        let up = self.p.trend || self.rng.unit() < 0.5 - drift.clamp(-0.4, 0.4);
        let spread = ba - bb;
        let inside = spread > 1 && self.rng.next_u64() & 1 == 0;  // a new level inside the spread
        match (up, inside) {
            (true, true) => { let k = 1 + self.rng.below((spread - 1).min(3) as u64) as Ticks; let q = self.qty(); self.emit(Side::Bid, bb + k, q) }
            (false, true) => { let k = 1 + self.rng.below((spread - 1).min(3) as u64) as Ticks; let q = self.qty(); self.emit(Side::Ask, ba - k, q) }
            (true, false) => self.emit(Side::Ask, ba, 0),         // the cursor walks out to the next ask
            (false, false) => self.emit(Side::Bid, bb, 0),        // and to the next bid
        }
        // a side never empties: refill one level behind the new best
        if self.asks.is_empty() { let q = self.qty(); let p = self.best_bid() + 1 + self.p.spread; self.emit(Side::Ask, p, q); }
        if self.bids.is_empty() { let q = self.qty(); let p = self.best_ask() - 1 - self.p.spread; self.emit(Side::Bid, p, q); }
    }
}

/// `p.records` updates, and what kinds they were.
pub fn generate(p: &Params) -> (Vec<Update>, Made) {
    let mut g = Gen { p, rng: Rng(p.seed), bids: BTreeMap::new(), asks: BTreeMap::new(),
                      out: Vec::with_capacity(p.records as usize),
                      made: Made { min_price: Ticks::MAX, max_price: Ticks::MIN, ..Default::default() },
                      first_mid2: 2 * p.best_bid + p.spread };
    // the opening book, inside out and alternating, so both sides exist from the second update
    for i in 0..p.depth {
        let q = g.qty();
        g.emit(Side::Bid, p.best_bid - i, q);
        let q = g.qty();
        g.emit(Side::Ask, p.best_bid + p.spread + i, q);
    }
    g.made.opening = g.out.len() as u64;
    while !g.full() {
        let u = g.rng.unit();
        if u < p.moves { g.best_move(); } else if u < p.moves + p.best_qty { g.best_qty(); } else { g.deep(); }
    }
    (g.out, g.made)
}

/// The property test's stream: random levels near `mid`, bids below it and
/// asks above, zero quantities one time in `zero_one_in`, so that the best
/// empties often and has to be found again across 64-bit words.
pub fn random(n: usize, seed: u64, mid: Ticks, band: Ticks, zero_one_in: u64) -> Vec<Update> {
    let mut rng = Rng(seed);
    (1..=n as u64).map(|seq| {
        let side = if rng.next_u64() & 1 == 0 { Side::Bid } else { Side::Ask };
        let depth = rng.below(band as u64) as Ticks;
        let price = match side { Side::Bid => mid - depth, Side::Ask => mid + 1 + depth };
        let qty = if rng.below(zero_one_in) == 0 { 0 } else { 1 + rng.below(900) as Lots };
        Update { seq, price, qty, instrument: 42, side }
    }).collect()
}

/// The mid of the first bid and the first ask: where the dense array's window is centred.
pub fn first_mid(updates: &[Update]) -> Option<Ticks> {
    let bid = updates.iter().find(|u| u.side == Side::Bid && u.qty != 0)?.price;
    let ask = updates.iter().find(|u| u.side == Side::Ask && u.qty != 0)?.price;
    Some((bid + ask) / 2)
}

// ---------------------------------------------------------------- the replay file

/// One record: arrival time in ns since the first record, then the 48 wire bytes.
pub const RECORD: usize = 56;

/// The networking chapter's 48-byte message for `u`, at the offsets it documents.
pub fn encode(u: &Update, ts_ns: u64) -> [u8; 48] {
    let mut m = [0u8; 48];
    m[0..2].copy_from_slice(&48u16.to_le_bytes());           // len
    m[2..4].copy_from_slice(&7u16.to_le_bytes());            // kind 7: a depth update
    m[4..8].copy_from_slice(&u.instrument.to_le_bytes());
    m[8..16].copy_from_slice(&u.seq.to_le_bytes());
    m[16..24].copy_from_slice(&u.price.to_le_bytes());
    m[24..32].copy_from_slice(&u.qty.to_le_bytes());
    m[32..40].copy_from_slice(&ts_ns.to_le_bytes());
    m[40] = match u.side { Side::Bid => 0, Side::Ask => 1 };
    m
}

/// The message back to an `Update`, or None for anything but a depth update.
pub fn decode(m: &[u8; 48]) -> Option<Update> {
    let u16_at = |i: usize| u16::from_le_bytes([m[i], m[i + 1]]);
    let u64_at = |i: usize| u64::from_le_bytes(m[i..i + 8].try_into().expect("8 bytes"));
    if u16_at(0) != 48 || u16_at(2) != 7 { return None; }
    let side = match m[40] { 0 => Side::Bid, 1 => Side::Ask, _ => return None };
    Some(Update { seq: u64_at(8), price: u64_at(16) as i64, qty: u64_at(24) as i64,
                  instrument: u32::from_le_bytes(m[4..8].try_into().expect("4 bytes")), side })
}

/// Write `updates` as 56-byte records, one every `gap_ns`.
pub fn write_records(path: &Path, updates: &[Update], gap_ns: u64) -> io::Result<()> {
    let mut w = BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    for (k, u) in updates.iter().enumerate() {
        let t = k as u64 * gap_ns;
        w.write_all(&t.to_le_bytes())?;
        w.write_all(&encode(u, 1_761_000_000_000_000_000 + t))?;
    }
    w.flush()
}

/// Read at most `max` depth updates from a file of 56-byte records, all into
/// memory before any timing.
pub fn read_records(path: &Path, max: usize) -> io::Result<Vec<Update>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut bytes)?;
    if !bytes.len().is_multiple_of(RECORD) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a whole number of 56-byte records"));
    }
    Ok(bytes.chunks_exact(RECORD).filter_map(|r| decode(r[8..].try_into().expect("48 bytes"))).take(max).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{crossed, Book, TreeBook};

    #[test]
    fn the_stream_has_the_shape_and_is_never_crossed() {
        let (v, made) = generate(&Params { records: 300_000, ..Default::default() });
        assert_eq!(v.len(), 300_000);
        let mut book = TreeBook::default();
        let (mut tops, mut moved) = (0u64, 0u64);
        for (k, u) in v.iter().enumerate() {
            assert_eq!(u.seq, k as u64 + 1);
            let before = book.top();
            book.apply(u);
            assert!(!crossed(&book), "crossed at seq {}", u.seq);
            if k >= 1 { assert!(book.best_bid().is_some() && book.best_ask().is_some()); }
            let after = book.top();
            tops += (after != before) as u64;
            moved += (after.bid.map(|b| b.0) != before.bid.map(|b| b.0) || after.ask.map(|a| a.0) != before.ask.map(|a| a.0)) as u64;
        }
        let n = (v.len() as u64 - made.opening) as f64;
        let pct = |x: u64| 100.0 * x as f64 / n;
        assert!((94.0..96.0).contains(&pct(made.deep)), "deep {:.2}%", pct(made.deep));
        assert!((3.5..4.5).contains(&pct(made.best_qty)), "best qty {:.2}%", pct(made.best_qty));
        assert!((0.8..1.2).contains(&pct(made.moves)), "moves {:.2}%", pct(made.moves));
        assert!(tops as f64 / n > 0.04 && (moved as f64 / n) < 0.02, "tops {tops} moved {moved}");
        assert!(made.max_price - made.min_price < 30_000, "inside the window");
    }

    #[test]
    fn the_same_seed_makes_the_same_stream() {
        let p = Params { records: 20_000, ..Default::default() };
        assert_eq!(generate(&p).0, generate(&p).0);
        assert_ne!(generate(&p).0, generate(&Params { seed: 8, ..p }).0);
    }

    #[test]
    fn a_trend_carries_the_price_up() {
        let (v, made) = generate(&Params { records: 200_000, moves: 0.2, trend: true, ..Default::default() });
        let mut book = TreeBook::default();
        for u in &v { book.apply(u); assert!(!crossed(&book)); }
        assert!(book.best_bid().expect("bids").0 > 6_812_345 + 10_000, "{made:?}");
    }

    #[test]
    fn records_round_trip() {
        let (v, _) = generate(&Params { records: 1_000, ..Default::default() });
        let path = std::env::temp_dir().join(format!("ch06-records-{}.bin", std::process::id()));
        write_records(&path, &v, 50_000).unwrap();
        let back = read_records(&path, usize::MAX).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(back, v);
    }
}

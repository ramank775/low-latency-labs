//! The book chapter's pieces: the `Book` trait and its three structures, the
//! sequence check, the feed handler that keeps the copy true, and the toy quoter;
//! then the lab's additions: the dense array's two re-bases and its rebuild,
//! `RingBook` (the window as a ring, the stretch), and the reference book's
//! levels as a snapshot. The generator of updates is in `gen`.
//!
//! The capstone feeds these with the data-structures chapter's 32-byte
//! `Update` (it carries `instrument`; the book chapter's own `Update` did not),
//! so that one type crosses from the decoder to the book.

use ch05_structures::lab::{new_hist, now, Clock};
use ch05_structures::{next_set_bit_above, next_set_bit_below};
use hdrhistogram::Histogram;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::hint::black_box;
use std::time::Instant;

pub mod gen;

pub use ch05_structures::{Side, Update};

pub type Ticks = i64;   // price as a count of ticks
pub type Lots  = i64;   // quantity as a count of lots

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub struct Top {
    pub bid: Option<(Ticks, Lots)>,
    pub ask: Option<(Ticks, Lots)>,
}

pub trait Book {
    /// Apply one level update. Returns true when the top of book changed.
    fn apply(&mut self, u: &Update) -> bool;
    fn best_bid(&self) -> Option<(Ticks, Lots)>;
    fn best_ask(&self) -> Option<(Ticks, Lots)>;
    fn top(&self) -> Top { Top { bid: self.best_bid(), ask: self.best_ask() } }
}

// ---------------------------------------------------------------- the dense array

pub const LEVELS: usize = 65_536;     // prices covered, in ticks

pub struct ArrayBook {
    base: Ticks,              // the price held at index 0
    bids: Vec<Lots>,          // bids[i]: quantity bid at price base + i
    asks: Vec<Lots>,          // asks[i]: quantity offered at price base + i
    bid_bits: Vec<u64>,       // bit i is set when bids[i] != 0
    ask_bits: Vec<u64>,       // bit i is set when asks[i] != 0
    best_bid: Option<usize>,  // index of the highest non-empty bid
    best_ask: Option<usize>,  // index of the lowest non-empty ask
}

impl ArrayBook {
    /// Every byte is allocated here, once; `apply` never allocates.
    pub fn new(base: Ticks) -> Self {
        ArrayBook {
            base,
            bids: vec![0; LEVELS], asks: vec![0; LEVELS],
            bid_bits: vec![0; LEVELS / 64], ask_bits: vec![0; LEVELS / 64],
            best_bid: None, best_ask: None,
        }
    }

    /// The price at index 0 of the window.
    pub fn base(&self) -> Ticks { self.base }
}

impl Book for ArrayBook {
    fn apply(&mut self, u: &Update) -> bool {
        let offset = u.price - self.base;          // index arithmetic
        if offset < 0 || offset >= LEVELS as Ticks {
            return false;                          // outside the range: see below
        }
        let i = offset as usize;
        let (levels, bits, best) = match u.side {
            Side::Bid => (&mut self.bids, &mut self.bid_bits, &mut self.best_bid),
            Side::Ask => (&mut self.asks, &mut self.ask_bits, &mut self.best_ask),
        };
        if levels[i] == u.qty {
            return false;                          // nothing changed
        }
        levels[i] = u.qty;
        let (word, bit) = (i / 64, i % 64);        // where level i's bit lives
        if u.qty != 0 { bits[word] |= 1 << bit; } else { bits[word] &= !(1 << bit); }

        let better = |a: usize, b: usize| match u.side { Side::Bid => a > b, Side::Ask => a < b };
        match *best {
            Some(c) if i == c && u.qty == 0 => {   // the best emptied: walk inward
                *best = match u.side {
                    Side::Bid => next_set_bit_below(bits, c),
                    Side::Ask => next_set_bit_above(bits, c),
                };
                true
            }
            Some(c) if i == c => true,             // the best's quantity changed
            Some(c) if !better(i, c) => false,     // a deeper level: top untouched
            _ => { *best = Some(i); true }         // a new best, or the side's first level
        }
    }
    fn best_bid(&self) -> Option<(Ticks, Lots)> {
        self.best_bid.map(|i| (self.base + i as Ticks, self.bids[i]))
    }
    fn best_ask(&self) -> Option<(Ticks, Lots)> {
        self.best_ask.map(|i| (self.base + i as Ticks, self.asks[i]))
    }
}

// ---------------------------------------------------------------- the window's edge: re-base and rebuild

/// The highest set bit of a whole bitmap, or None.
fn highest_bit(bits: &[u64]) -> Option<usize> {
    let top = bits.len() * 64 - 1;
    if bits[top / 64] >> 63 == 1 { Some(top) } else { next_set_bit_below(bits, top) }
}

/// The lowest set bit of a whole bitmap, or None.
fn lowest_bit(bits: &[u64]) -> Option<usize> {
    if bits[0] & 1 == 1 { Some(0) } else { next_set_bit_above(bits, 0) }
}

/// New bit i is old bit i + d: the bitmap's half of a copy re-base upward.
fn shift_bits_down(bits: &mut [u64], d: usize) {
    let (q, r) = (d / 64, d % 64);
    for w in 0..bits.len() {                       // ascending: each read is at or above the write
        let lo = bits.get(w + q).copied().unwrap_or(0);
        let hi = bits.get(w + q + 1).copied().unwrap_or(0);
        bits[w] = if r == 0 { lo } else { (lo >> r) | (hi << (64 - r)) };
    }
}

/// New bit i is old bit i - e: a copy re-base downward.
fn shift_bits_up(bits: &mut [u64], e: usize) {
    let (q, r) = (e / 64, e % 64);
    for w in (0..bits.len()).rev() {               // descending: each read is at or below the write
        let hi = if w >= q { bits[w - q] } else { 0 };
        let lo = if w > q { bits[w - q - 1] } else { 0 };
        bits[w] = if r == 0 { hi } else { (hi << r) | (lo >> (64 - r)) };
    }
}

/// Clear bits lo..hi, a word at a time.
fn clear_bits(bits: &mut [u64], lo: usize, hi: usize) {
    if lo >= hi { return; }
    let (wl, wh) = (lo / 64, (hi - 1) / 64);
    let lo_mask = u64::MAX << (lo % 64);           // lo and above, in its word
    let hi_mask = u64::MAX >> (63 - (hi - 1) % 64); // hi - 1 and below, in its word
    if wl == wh { bits[wl] &= !(lo_mask & hi_mask); return; }
    bits[wl] &= !lo_mask;
    for w in &mut bits[wl + 1..wh] { *w = 0; }
    bits[wh] &= !hi_mask;
}

impl ArrayBook {
    /// The copy re-base: move the window so index 0 holds `new_base`, every
    /// level copied along by the distance moved (a `memmove` of up to 512 KB a
    /// side) and the bitmaps shifted with them. The levels entering read zero:
    /// their updates were ignored while they were outside.
    pub fn rebase_copy(&mut self, new_base: Ticks) {
        let d = new_base - self.base;
        if d == 0 { return; }
        if d.unsigned_abs() >= LEVELS as u64 {     // nothing survives: clear it all
            for v in [&mut self.bids, &mut self.asks] { v.fill(0); }
            for b in [&mut self.bid_bits, &mut self.ask_bits] { b.fill(0); }
            (self.best_bid, self.best_ask) = (None, None);
        } else if d > 0 {                          // up: the lowest d prices leave
            let d = d as usize;
            for v in [&mut self.bids, &mut self.asks] { v.copy_within(d.., 0); v[LEVELS - d..].fill(0); }
            for b in [&mut self.bid_bits, &mut self.ask_bits] { shift_bits_down(b, d); }
            self.best_bid = self.best_bid.and_then(|i| i.checked_sub(d));  // below the window: no bids left
            self.best_ask = match self.best_ask {
                Some(i) if i >= d => Some(i - d),
                Some(_) => lowest_bit(&self.ask_bits),  // the best left: the lowest that stayed
                None => None,
            };
        } else {                                   // down: the highest e prices leave
            let e = (-d) as usize;
            for v in [&mut self.bids, &mut self.asks] { v.copy_within(..LEVELS - e, e); v[..e].fill(0); }
            for b in [&mut self.bid_bits, &mut self.ask_bits] { shift_bits_up(b, e); }
            self.best_bid = match self.best_bid {
                Some(i) if i + e < LEVELS => Some(i + e),
                Some(_) => highest_bit(&self.bid_bits),
                None => None,
            };
            self.best_ask = self.best_ask.map(|i| i + e).filter(|&i| i < LEVELS);
        }
        self.base = new_base;
    }

    /// The quantity held at `price`, or None outside the window: for diffs.
    pub fn qty_at(&self, side: Side, price: Ticks) -> Option<Lots> {
        let i = usize::try_from(price - self.base).ok().filter(|&i| i < LEVELS)?;
        Some(match side { Side::Bid => self.bids[i], Side::Ask => self.asks[i] })
    }

    /// The rebuild: every level cleared, the window moved to `new_base`, and
    /// the levels of a book known to be right applied, as from a snapshot.
    /// No holes; in production the snapshot's round trip is the cost.
    pub fn rebuild(&mut self, new_base: Ticks, levels: &[Update]) {
        for v in [&mut self.bids, &mut self.asks] { v.fill(0); }
        for b in [&mut self.bid_bits, &mut self.ask_bits] { b.fill(0); }
        (self.best_bid, self.best_ask, self.base) = (None, None, new_base);
        for l in levels { self.apply(l); }
    }
}

// ---------------------------------------------------------------- the stretch: the window as a ring

const MASK: usize = LEVELS - 1;

/// A price's slot in the ring: its low 16 bits, the extra AND per update.
#[inline(always)]
fn slot(p: Ticks) -> usize { (p as u64 as usize) & MASK }

/// The price in slot `s` of the window that starts at `base`.
#[inline(always)]
fn price_at(base: Ticks, s: usize) -> Ticks { base + (s.wrapping_sub(slot(base)) & MASK) as Ticks }

/// The next set bit below slot `s`, wrapping from slot 0 to the top: in
/// prices, the next lower level, given that no set bit lies above `s`'s price.
fn ring_below(bits: &[u64], s: usize) -> Option<usize> {
    next_set_bit_below(bits, s).or_else(|| {
        if s < MASK && bits[MASK / 64] >> 63 == 1 { return Some(MASK); }
        next_set_bit_below(bits, MASK).filter(|&x| x > s)
    })
}

/// The next set bit above slot `s`, wrapping from the top to slot 0.
fn ring_above(bits: &[u64], s: usize) -> Option<usize> {
    next_set_bit_above(bits, s).or_else(|| {
        if s > 0 && bits[0] & 1 == 1 { return Some(0); }
        next_set_bit_above(bits, 0).filter(|&x| x < s)
    })
}

/// The dense array with its window as a ring: price p lives in slot p mod
/// 65,536, so moving the window d ticks hands the d prices entering the slots
/// of the d that left, and a re-base is a new base plus clearing those slots
/// and their bits. The cursors hold prices, since slot order wraps.
pub struct RingBook {
    base: Ticks,              // the window is base .. base + LEVELS
    bids: Vec<Lots>,          // bids[slot(p)]: quantity bid at price p
    asks: Vec<Lots>,
    bid_bits: Vec<u64>,       // bit slot(p) set when bids[slot(p)] != 0
    ask_bits: Vec<u64>,
    best_bid: Option<Ticks>,  // the highest bid's price
    best_ask: Option<Ticks>,  // the lowest ask's price
}

impl RingBook {
    /// Every byte is allocated here, once; neither `apply` nor `rebase` allocates.
    pub fn new(base: Ticks) -> Self {
        RingBook {
            base,
            bids: vec![0; LEVELS], asks: vec![0; LEVELS],
            bid_bits: vec![0; LEVELS / 64], ask_bits: vec![0; LEVELS / 64],
            best_bid: None, best_ask: None,
        }
    }

    pub fn base(&self) -> Ticks { self.base }

    /// Clear `n` prices from `from` up, both sides: one or two runs of slots.
    fn clear_prices(&mut self, from: Ticks, n: usize) {
        let start = slot(from);
        let first = n.min(LEVELS - start);
        for (lo, hi) in [(start, start + first), (0, n - first)] {
            if lo == hi { continue; }
            self.bids[lo..hi].fill(0);
            self.asks[lo..hi].fill(0);
            clear_bits(&mut self.bid_bits, lo, hi);
            clear_bits(&mut self.ask_bits, lo, hi);
        }
    }

    /// Move the window to start at `new_base`. A step of d ticks clears d
    /// slots a side; a jump of a whole window or more clears them all.
    pub fn rebase(&mut self, new_base: Ticks) {
        let d = new_base - self.base;
        if d == 0 { return; }
        if d.unsigned_abs() >= LEVELS as u64 {
            self.clear_prices(self.base, LEVELS);
        } else if d > 0 {
            self.clear_prices(self.base, d as usize);                       // the lowest d leave
        } else {
            self.clear_prices(self.base + LEVELS as Ticks + d, (-d) as usize); // the highest leave
        }
        self.base = new_base;
        let top = slot(new_base + LEVELS as Ticks - 1);
        if self.best_bid.is_some_and(|p| !self.in_window(p)) {              // the best left: the highest that stayed
            let found = if self.bid_bits[top / 64] >> (top % 64) & 1 == 1 { Some(top) } else { ring_below(&self.bid_bits, top) };
            self.best_bid = found.map(|s| price_at(new_base, s));
        }
        if self.best_ask.is_some_and(|p| !self.in_window(p)) {
            let bottom = slot(new_base);
            let found = if self.ask_bits[bottom / 64] >> (bottom % 64) & 1 == 1 { Some(bottom) } else { ring_above(&self.ask_bits, bottom) };
            self.best_ask = found.map(|s| price_at(new_base, s));
        }
    }

    fn in_window(&self, p: Ticks) -> bool { p >= self.base && p < self.base + LEVELS as Ticks }

    /// The quantity held at `price`, or None outside the window: for diffs.
    pub fn qty_at(&self, side: Side, price: Ticks) -> Option<Lots> {
        self.in_window(price).then(|| match side { Side::Bid => self.bids[slot(price)], Side::Ask => self.asks[slot(price)] })
    }
}

impl Book for RingBook {
    fn apply(&mut self, u: &Update) -> bool {
        if u.price < self.base || u.price >= self.base + LEVELS as Ticks {
            return false;                          // outside the window
        }
        let (i, base) = (slot(u.price), self.base);
        let (levels, bits, best) = match u.side {
            Side::Bid => (&mut self.bids, &mut self.bid_bits, &mut self.best_bid),
            Side::Ask => (&mut self.asks, &mut self.ask_bits, &mut self.best_ask),
        };
        if levels[i] == u.qty {
            return false;                          // nothing changed
        }
        levels[i] = u.qty;
        let (word, bit) = (i / 64, i % 64);
        if u.qty != 0 { bits[word] |= 1 << bit; } else { bits[word] &= !(1 << bit); }

        let better = |a: Ticks, b: Ticks| match u.side { Side::Bid => a > b, Side::Ask => a < b };
        match *best {
            Some(c) if u.price == c && u.qty == 0 => { // the best emptied: walk inward, wrapping
                let next = match u.side { Side::Bid => ring_below(bits, i), Side::Ask => ring_above(bits, i) };
                *best = next.map(|s| price_at(base, s));
                true
            }
            Some(c) if u.price == c => true,
            Some(c) if !better(u.price, c) => false,
            _ => { *best = Some(u.price); true }
        }
    }
    fn best_bid(&self) -> Option<(Ticks, Lots)> { self.best_bid.map(|p| (p, self.bids[slot(p)])) }
    fn best_ask(&self) -> Option<(Ticks, Lots)> { self.best_ask.map(|p| (p, self.asks[slot(p)])) }
}

// ---------------------------------------------------------------- the tree

#[derive(Default)]
pub struct TreeBook {
    bids: BTreeMap<Ticks, Lots>,
    asks: BTreeMap<Ticks, Lots>,
}

impl TreeBook {
    fn best(&self, side: Side) -> Option<(Ticks, Lots)> {
        let found = match side {
            Side::Bid => self.bids.last_key_value(),  // highest bid
            Side::Ask => self.asks.first_key_value(), // lowest ask
        };
        found.map(|(&p, &q)| (p, q))
    }
}

impl Book for TreeBook {
    fn apply(&mut self, u: &Update) -> bool {
        let before = self.best(u.side);
        let map = match u.side { Side::Bid => &mut self.bids, Side::Ask => &mut self.asks };
        if u.qty == 0 { map.remove(&u.price); } else { map.insert(u.price, u.qty); }
        self.best(u.side) != before
    }
    fn best_bid(&self) -> Option<(Ticks, Lots)> { self.best(Side::Bid) }
    fn best_ask(&self) -> Option<(Ticks, Lots)> { self.best(Side::Ask) }
}

impl TreeBook {
    /// Every level, bids then asks, as updates numbered 0: what a snapshot
    /// of the venue's book would hand a rebuild or a resync.
    pub fn snapshot(&self) -> Vec<Update> {
        let level = |side, (&price, &qty): (&Ticks, &Lots)| Update { seq: 0, price, qty, instrument: 42, side };
        self.bids.iter().map(|l| level(Side::Bid, l)).chain(self.asks.iter().map(|l| level(Side::Ask, l))).collect()
    }

    /// Levels a side: bids, asks.
    pub fn depth(&self) -> (usize, usize) { (self.bids.len(), self.asks.len()) }
}

// ---------------------------------------------------------------- the sorted Vec

#[derive(Default)]
pub struct VecBook {
    bids: Vec<(Ticks, Lots)>, // descending: the highest bid at index 0
    asks: Vec<(Ticks, Lots)>, // ascending: the lowest ask at index 0
}

impl VecBook {
    /// Room for `levels` a side reserved now, so an insert moves bytes but never allocates.
    pub fn with_capacity(levels: usize) -> Self {
        VecBook { bids: Vec::with_capacity(levels), asks: Vec::with_capacity(levels) }
    }
}

impl Book for VecBook {
    fn apply(&mut self, u: &Update) -> bool {
        let levels = match u.side { Side::Bid => &mut self.bids, Side::Ask => &mut self.asks };
        let before = levels.first().copied();
        let found = levels.binary_search_by(|&(p, _)| match u.side {
            Side::Bid => u.price.cmp(&p), // descending order
            Side::Ask => p.cmp(&u.price), // ascending order
        });
        match (found, u.qty) {
            (Ok(i), 0) => { levels.remove(i); }           // level gone: close the gap
            (Ok(i), q) => levels[i].1 = q,                 // level exists: overwrite
            (Err(_), 0) => {}                              // removing a level we never had
            (Err(i), q) => levels.insert(i, (u.price, q)), // new level: open a gap
        }
        levels.first().copied() != before
    }
    fn best_bid(&self) -> Option<(Ticks, Lots)> { self.bids.first().copied() }
    fn best_ask(&self) -> Option<(Ticks, Lots)> { self.asks.first().copied() }
}

// ---------------------------------------------------------------- keeping the book true

pub enum Seq { Next, Seen, Gap { expected: u64, got: u64 } }

pub fn check_seq(last: u64, seq: u64) -> Seq {
    match seq.cmp(&(last + 1)) {
        Ordering::Equal   => Seq::Next,
        Ordering::Less    => Seq::Seen,
        Ordering::Greater => Seq::Gap { expected: last + 1, got: seq },
    }
}

/// A crossed copy is a wrong copy: the venue's own book can never be crossed.
pub fn crossed(book: &impl Book) -> bool {
    matches!((book.best_bid(), book.best_ask()), (Some((bid, _)), Some((ask, _))) if bid >= ask)
}

/// The venue's join rule, here for a stream numbered one update at a time:
/// the first delta to apply after snapshot `snapshot_id` is the one numbered next.
pub fn joins(snapshot_id: u64, u: &Update) -> bool {
    u.seq == snapshot_id + 1
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BookState { Live, Stale, Syncing { since_seq: u64 } }

/// The feed handler, grown for recovery. Its fields are public so that a
/// caller who knows where the stream starts, as a replay does, can build it whole.
pub struct FeedHandler<B: Book> {
    pub book: B,
    pub state: BookState,
    pub next_seq: u64,       // the number the next update must carry
    pub buffer: Vec<Update>, // deltas held while not Live; capacity reserved at startup
}

impl<B: Book> FeedHandler<B> {
    /// A handler for a live feed: Stale until a snapshot is joined, with the
    /// 4,096-update buffer reserved now so that going Stale never allocates.
    pub fn new(book: B) -> Self {
        FeedHandler { book, state: BookState::Stale, next_seq: 0, buffer: Vec::with_capacity(4096) }
    }

    /// Returns the new top when it changed and can be trusted.
    pub fn on_update(&mut self, u: Update) -> Option<Top> {
        if u.qty < 0 {                                   // a bad quantity: never applied, never held
            self.state = BookState::Stale;
            self.buffer.clear();
            return None;
        }
        if let BookState::Live = self.state {
            if u.seq < self.next_seq { return None; }    // a repeat: already applied
            if u.seq == self.next_seq {
                self.next_seq = u.seq + 1;
                let changed = self.book.apply(&u);
                if !crossed(&self.book) { return changed.then(|| self.book.top()); }
            }
            self.state = BookState::Stale;               // gap or cross
            self.buffer.clear();
        }
        if self.buffer.len() == self.buffer.capacity() { // resync too slow: start again
            self.state = BookState::Stale;
            self.buffer.clear();
        }
        self.buffer.push(u);
        None
    }
}

impl<B: Book + Default> FeedHandler<B> {
    pub fn on_snapshot(&mut self, snapshot_id: u64, levels: &[Update]) -> Option<Top> {
        let BookState::Syncing { since_seq } = self.state else { return None };
        self.state = BookState::Stale;                   // until the join succeeds
        if since_seq > snapshot_id + 1 { return None; }  // snapshot older than the buffer
        self.book = B::default();                        // empty, then the snapshot
        for l in levels { self.book.apply(l); }
        self.next_seq = snapshot_id + 1;
        for u in self.buffer.drain(..) {
            if u.seq < self.next_seq { continue; }       // in the snapshot, or a repeat: drop
            let first = self.next_seq == snapshot_id + 1;
            if (first && !joins(snapshot_id, &u)) || (!first && u.seq != self.next_seq) {
                return None;                             // a hole: ask again
            }
            self.book.apply(&u);
            self.next_seq = u.seq + 1;
        }
        if crossed(&self.book) { return None; }
        self.state = BookState::Live;
        Some(self.book.top())
    }
}

// ---------------------------------------------------------------- the replay harness

/// What a timed replay measured: `apply` in nanoseconds, the clock pair not
/// yet subtracted, all together and by what the update did to the top.
pub struct Timed {
    pub all: Histogram<u64>,
    pub unchanged: Histogram<u64>,   // the top as it was: a deep level, most updates
    pub best_qty: Histogram<u64>,    // the same best prices, a quantity changed
    pub moved: Histogram<u64>,       // a best price moved: the cursor's walk, if any
    pub top_changes: u64,            // apply returned true
}

/// Feed `updates` through `book`, each `apply` between two fenced `rdtsc`
/// reads, the first `warmup` applied but not recorded. The top is read before
/// and after each apply, outside the timed region, to sort the samples.
pub fn timed_replay<B: Book>(book: &mut B, updates: &[Update], warmup: usize, clock: &Clock) -> Timed {
    let mut t = Timed { all: new_hist(), unchanged: new_hist(), best_qty: new_hist(), moved: new_hist(), top_changes: 0 };
    for (k, u) in updates.iter().enumerate() {
        let before = book.top();
        let t0 = now();
        let changed = book.apply(u);
        let t1 = now();
        let after = book.top();
        if k < warmup { continue; }
        let ns = clock.ns(t1 - t0);
        t.all.saturating_record(ns);
        t.top_changes += changed as u64;
        let price = |x: Top| (x.bid.map(|l| l.0), x.ask.map(|l| l.0));
        let h = if after == before { &mut t.unchanged } else if price(after) == price(before) { &mut t.best_qty } else { &mut t.moved };
        h.saturating_record(ns);
    }
    t
}

/// The same feed with no clock and no top reads: updates per second, and the
/// top changes, which `black_box` keeps the compiler from deleting.
pub fn untimed_replay<B: Book>(book: &mut B, updates: &[Update]) -> (f64, u64) {
    let start = Instant::now();
    let mut changes = 0u64;
    for u in updates { changes += black_box(book.apply(black_box(u))) as u64; }
    (updates.len() as f64 / start.elapsed().as_secs_f64(), changes)
}

// ---------------------------------------------------------------- the quoter

/// A cancel-replace: move my resting order on `side` to `price` for `qty`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Order { pub side: Side, pub price: Ticks, pub qty: Lots }

pub struct Quoter {
    pub inventory: Lots,                     // lots bought minus lots sold; > 0 is long
    pub half_spread: Ticks,                  // distance from the mid to each quote
    pub skew_per_lot: Ticks,                 // how far both quotes move per lot held
    pub size: Lots,                          // lots on each side
    pub resting: [Option<(Ticks, Lots)>; 2], // [bid, ask] as last sent
}

impl Quoter {
    /// Runs on the book's thread, after apply, only when the top changed.
    pub fn on_top(&mut self, top: Top) -> [Option<Order>; 2] {
        let (Some((bid, _)), Some((ask, _))) = (top.bid, top.ask) else {
            return [None, None];                 // a side is empty: the toy waits
        };
        let mid2 = bid + ask;                    // twice the mid, always an integer
        let skew2 = 2 * self.inventory * self.skew_per_lot;
        let want = [
            (mid2 - 2 * self.half_spread - skew2).div_euclid(2),     // bid, rounded down
            (mid2 + 2 * self.half_spread - skew2 + 1).div_euclid(2), // ask, rounded up
        ];
        let mut out = [None, None];
        for (i, side) in [Side::Bid, Side::Ask].into_iter().enumerate() {
            let desired = Some((want[i], self.size));
            if self.resting[i] != desired {
                out[i] = Some(Order { side, price: want[i], qty: self.size });
                self.resting[i] = desired;       // assumed accepted
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MID: Ticks = 6_812_345;

    fn up(seq: u64, side: Side, price: Ticks, qty: Lots) -> Update {
        Update { seq, price, qty, instrument: 42, side }
    }

    /// A deterministic stream near one price, with zero quantities common
    /// enough that the best empties and has to be found again.
    fn stream(n: usize, seed: u64) -> Vec<Update> {
        let mut x = seed | 1;
        let mut next = move || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
        (1..=n as u64).map(|seq| {
            let r = next();
            let side = if r & 1 == 0 { Side::Bid } else { Side::Ask };
            // bids below the mid and asks above it, so the book never crosses
            let depth = (r >> 8) as i64 % 40;
            let price = match side { Side::Bid => MID - depth, Side::Ask => MID + 1 + depth };
            let qty = if (r >> 20) % 3 == 0 { 0 } else { ((r >> 24) % 900) as i64 + 1 };
            up(seq, side, price, qty)
        }).collect()
    }

    #[test]
    fn three_books_give_identical_tops_after_every_update() {
        for seed in [1u64, 7, 42, 0x5eed] {
            let mut a = ArrayBook::new(MID - (LEVELS / 2) as Ticks);
            let mut t = TreeBook::default();
            let mut v = VecBook::default();
            for u in stream(20_000, seed) {
                let (ca, ct, cv) = (a.apply(&u), t.apply(&u), v.apply(&u));
                assert_eq!((ca, ct), (ct, cv), "apply's answer differs at seq {}", u.seq);
                assert_eq!(a.top(), t.top(), "array vs tree at seq {}", u.seq);
                assert_eq!(v.top(), t.top(), "vec vs tree at seq {}", u.seq);
            }
        }
    }

    #[test]
    fn array_ignores_prices_outside_its_window() {
        let mut a = ArrayBook::new(MID);
        assert!(!a.apply(&up(1, Side::Bid, MID - 1, 10)));
        assert!(!a.apply(&up(2, Side::Ask, MID + LEVELS as Ticks, 10)));
        assert_eq!(a.top(), Top::default());
    }

    #[test]
    fn check_seq_names_the_three_cases() {
        assert!(matches!(check_seq(10, 11), Seq::Next));
        assert!(matches!(check_seq(10, 9), Seq::Seen));
        assert!(matches!(check_seq(10, 14), Seq::Gap { expected: 11, got: 14 }));
    }

    fn live<B: Book>(book: B, next_seq: u64) -> FeedHandler<B> {
        FeedHandler { book, state: BookState::Live, next_seq, buffer: Vec::with_capacity(4096) }
    }

    #[test]
    fn handler_applies_in_order_and_drops_repeats() {
        let mut h = live(ArrayBook::new(MID - 1000), 1);
        assert_eq!(h.on_update(up(1, Side::Bid, MID, 5)), Some(Top { bid: Some((MID, 5)), ask: None }));
        assert_eq!(h.on_update(up(2, Side::Bid, MID - 3, 5)), None);   // deep: top unchanged
        assert_eq!(h.on_update(up(2, Side::Bid, MID, 9)), None);       // a repeat: dropped
        assert_eq!(h.book.top().bid, Some((MID, 5)));
        assert_eq!(h.state, BookState::Live);
    }

    #[test]
    fn a_gap_takes_the_handler_out_of_live() {
        let mut h = live(ArrayBook::new(MID - 1000), 1);
        assert!(h.on_update(up(1, Side::Bid, MID, 5)).is_some());
        assert_eq!(h.on_update(up(3, Side::Ask, MID + 1, 5)), None);   // 2 is missing
        assert_eq!(h.state, BookState::Stale);
        assert_eq!(h.buffer.len(), 1);
        assert_eq!(h.book.top().ask, None, "nothing applied while not Live");
    }

    #[test]
    fn a_cross_or_a_negative_quantity_takes_it_out_of_live() {
        let mut h = live(TreeBook::default(), 1);
        h.on_update(up(1, Side::Bid, MID, 5));
        assert_eq!(h.on_update(up(2, Side::Ask, MID, 5)), None);       // ask at the bid
        assert_eq!(h.state, BookState::Stale);
        let mut h = live(TreeBook::default(), 1);
        assert_eq!(h.on_update(up(1, Side::Bid, MID, -1)), None);
        assert_eq!(h.state, BookState::Stale);
        h.on_update(up(2, Side::Bid, MID, 5));
        assert_eq!(h.on_update(up(3, Side::Bid, MID, -1)), None);
        assert!(h.buffer.is_empty(), "a bad quantity is never held for the replay");
    }

    #[test]
    fn the_buffer_never_grows() {
        let mut h = FeedHandler::new(TreeBook::default());
        let cap = h.buffer.capacity();
        for s in 0..10_000 { h.on_update(up(s, Side::Bid, MID, 1)); }
        assert_eq!(h.buffer.capacity(), cap);
        assert!(h.buffer.len() <= cap);
    }

    #[test]
    fn resync_joins_a_snapshot_to_the_buffered_deltas() {
        let all = stream(3_000, 99);
        let mut reference = TreeBook::default();
        for u in &all { reference.apply(u); }

        // the handler misses everything up to seq 2,000, buffers from 1,990,
        // and is handed a snapshot taken at 2,000
        let mut h = FeedHandler::new(TreeBook::default());
        for u in &all[1_989..] { h.on_update(*u); }
        assert_eq!(h.state, BookState::Stale);
        h.state = BookState::Syncing { since_seq: 1_990 };
        let mut at_2000 = TreeBook::default();
        for u in &all[..2_000] { at_2000.apply(u); }
        let snapshot: Vec<Update> = at_2000.bids.iter().map(|(&p, &q)| up(0, Side::Bid, p, q))
            .chain(at_2000.asks.iter().map(|(&p, &q)| up(0, Side::Ask, p, q))).collect();
        let top = h.on_snapshot(2_000, &snapshot);
        assert_eq!(h.state, BookState::Live);
        assert_eq!(top, Some(reference.top()));
        assert_eq!(h.next_seq, 3_001);
    }

    #[test]
    fn quoter_rounds_outward_and_sends_only_changes() {
        let mut q = Quoter { inventory: 0, half_spread: 2, skew_per_lot: 0, size: 100, resting: [None, None] };
        let top = Top { bid: Some((MID, 3)), ask: Some((MID + 1, 4)) };
        let first = q.on_top(top);
        assert_eq!(first, [Some(Order { side: Side::Bid, price: MID - 2, qty: 100 }),
                           Some(Order { side: Side::Ask, price: MID + 3, qty: 100 })]);
        assert_eq!(q.on_top(Top { bid: Some((MID, 9)), ..top }), [None, None]); // same mid
        assert_eq!(q.on_top(Top { bid: None, ..top }), [None, None]);            // a side empty
    }

    /// The exact model of a windowed book: a tree that ignores prices outside
    /// the window and forgets the levels that leave it when the window moves.
    #[derive(Default)]
    struct Windowed { base: Ticks, t: TreeBook }

    impl Windowed {
        fn apply(&mut self, u: &Update) {
            if u.price >= self.base && u.price < self.base + LEVELS as Ticks { self.t.apply(u); }
        }
        fn rebase(&mut self, base: Ticks) {
            self.base = base;
            let r = base..base + LEVELS as Ticks;
            self.t.bids.retain(|p, _| r.contains(p));
            self.t.asks.retain(|p, _| r.contains(p));
        }
    }

    #[test]
    fn ring_and_both_rebases_match_the_windowed_model_after_every_update() {
        for seed in [3u64, 7, 11] {
            let mut x = seed;
            let mut next = move || { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x };
            let base0 = MID - (LEVELS / 2) as Ticks;
            let (mut model, mut ring, mut arr) = (Windowed { base: base0, ..Default::default() }, RingBook::new(base0), ArrayBook::new(base0));
            for (k, u) in gen::random(30_000, seed, MID, 300, 3).into_iter().enumerate() {
                if k % 97 == 0 {                    // a step, now and then a jump away and back
                    let r = next();
                    let d = match r % 10 { 0 => LEVELS as Ticks + (r >> 8) as Ticks % 5, 1 => -(LEVELS as Ticks), _ => (r >> 8) as Ticks % 401 - 200 };
                    let nb = (model.base + d).clamp(MID - LEVELS as Ticks + 150, MID - 150);   // keep the band in sight
                    model.rebase(nb); ring.rebase(nb); arr.rebase_copy(nb);
                    assert_eq!((ring.top(), arr.top()), (model.t.top(), model.t.top()), "after rebase to {nb}, seed {seed}");
                }
                let (cr, ca) = (ring.apply(&u), arr.apply(&u));
                let before = model.t.top();
                model.apply(&u);
                let cm = model.t.top() != before;
                assert_eq!((cr, ca), (cm, cm), "apply's answer at seq {}, seed {seed}", u.seq);
                assert_eq!(ring.top(), model.t.top(), "ring at seq {}, seed {seed}", u.seq);
                assert_eq!(arr.top(), model.t.top(), "array at seq {}, seed {seed}", u.seq);
            }
        }
    }

    #[test]
    fn the_ring_wraps_its_walks_across_slot_zero() {
        let base = 3 * LEVELS as Ticks - 10;        // slot 0 is price base + 10
        let mut r = RingBook::new(base);
        let mut t = TreeBook::default();
        for (seq, (side, p, q)) in [(Side::Bid, base + 12, 5), (Side::Bid, base + 3, 7), (Side::Ask, base + 20, 1),
                                    (Side::Ask, base + 9, 2), (Side::Bid, base + 12, 0), (Side::Ask, base + 9, 0)].into_iter().enumerate() {
            let u = up(seq as u64 + 1, side, p, q);
            assert_eq!(r.apply(&u), t.apply(&u), "seq {}", seq + 1);
            assert_eq!(r.top(), t.top(), "seq {}", seq + 1);
        }
        assert_eq!(r.top(), Top { bid: Some((base + 3, 7)), ask: Some((base + 20, 1)) });
    }

    #[test]
    fn a_rebuild_has_no_holes() {
        let mut reference = TreeBook::default();
        for u in stream(5_000, 5) { reference.apply(&u); }
        let mut a = ArrayBook::new(MID + 1);        // every bid outside: ignored
        for u in stream(5_000, 5) { a.apply(&u); }
        assert_ne!(a.top(), reference.top());
        a.rebuild(MID - (LEVELS / 2) as Ticks, &reference.snapshot());
        assert_eq!(a.top(), reference.top());
        assert_eq!(reference.snapshot().len(), reference.depth().0 + reference.depth().1);
    }

    #[test]
    fn bit_shifts_and_clears() {
        let mut b = vec![0u64; 4];
        b[1] = 1 << 5;                              // bit 69
        shift_bits_down(&mut b, 66);
        assert_eq!(b, [1 << 3, 0, 0, 0]);
        shift_bits_up(&mut b, 130);
        assert_eq!(b, [0, 0, 1 << 5, 0]);           // bit 133
        let mut c = vec![u64::MAX; 4];
        clear_bits(&mut c, 3, 200);
        assert_eq!(c, [0b111, 0, 0, u64::MAX << 8]);
        assert_eq!((highest_bit(&c), lowest_bit(&c)), (Some(255), Some(0)));
    }
}

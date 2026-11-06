//! The data-structures chapter's pieces, as the capstone reuses them: the SPSC
//! ring, the 32-byte `Update`, the bitmap helpers, the pool, and the counting
//! allocator that proves a loop allocates nothing; then the rest of the
//! chapter's code: the refusing push, `Symbol`, `Interner` and `Positions`,
//! `Pending`, the `Strategy` of the bounds-check example, and the hand-written
//! FxHash the lookup lab measures.
//!
//! The lab's own machinery is in `lab` (clock, histograms, pinning, flags) and
//! `bench` (the two-thread queue harness that `ring`, `pad` and `alloc` share);
//! the arena of 5.3 is in `arena`, and the first, allocating `Update` of 5.1 in
//! `first_version`.

pub mod arena;
pub mod bench;
pub mod counting;
pub mod first_version;
pub mod lab;
mod ring;

pub use ring::{ring, Consumer, Producer, Ring};

use std::collections::HashMap;
use std::hash::Hasher;
use std::mem::MaybeUninit;

/// Bid or ask: a one-byte enum.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Side { Bid, Ask }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Update {            // fixed: 32 bytes, nothing on the heap
    pub seq: u64,              // the venue's update number
    pub price: i64,            // ticks
    pub qty: i64,              // lots; 0: none left at this price
    pub instrument: u32,       // a small id for the symbol, 42 here, given at startup
    pub side: Side,            // a one-byte enum, Bid or Ask; three bytes of padding follow
}

const _: () = assert!(size_of::<Update>() == 32);

/// The lowest set bit strictly above `i`, or `None`; `i` must be below `bits.len() * 64`.
pub fn next_set_bit_above(bits: &[u64], i: usize) -> Option<usize> {
    let mut w = i / 64;
    let mut word = bits[w] & ((u64::MAX << (i % 64)) << 1); // bits above i; 0 at bit 63
    loop {
        if word != 0 {
            return Some(w * 64 + word.trailing_zeros() as usize);
        }
        w += 1;
        if w == bits.len() { return None; }                  // ran off the top
        word = bits[w];
    }
}

/// The highest set bit strictly below `i`, or `None`.
pub fn next_set_bit_below(bits: &[u64], i: usize) -> Option<usize> {
    let mut w = i / 64;
    let mut word = bits[w] & ((1u64 << (i % 64)) - 1);      // bits below i; 0 at bit 0
    loop {
        if word != 0 {
            return Some(w * 64 + 63 - word.leading_zeros() as usize);
        }
        if w == 0 { return None; }                           // ran off the bottom
        w -= 1;
        word = bits[w];
    }
}

pub struct Pool<T> {
    slots: Vec<MaybeUninit<T>>, // room for n values, allocated once
    free: Vec<u32>,             // a stack of the indices of empty slots
    live: Vec<bool>,            // live[i]: slot i holds a value
}

impl<T> Pool<T> {
    pub fn with_capacity(n: usize) -> Self {
        assert!(n <= u32::MAX as usize);
        let mut slots = Vec::with_capacity(n);
        slots.resize_with(n, MaybeUninit::uninit);
        let free = (0..n as u32).rev().collect(); // slot 0 on top
        Pool { slots, free, live: vec![false; n] }
    }

    /// A slot for `v`, or `None` when all n are in use: the pool never grows.
    pub fn alloc(&mut self, v: T) -> Option<u32> {
        let i = self.free.pop()?;
        self.slots[i as usize].write(v);
        self.live[i as usize] = true;
        Some(i)
    }

    pub fn get(&self, i: u32) -> &T {
        assert!(self.live[i as usize], "slot {i} is empty");
        // SAFETY: live[i] is true only while slot i holds a value from alloc.
        unsafe { self.slots[i as usize].assume_init_ref() }
    }

    pub fn free(&mut self, i: u32) {
        assert!(self.live[i as usize], "slot {i} freed twice");
        self.live[i as usize] = false;
        // SAFETY: as in get; the flag is cleared first, so this runs once.
        unsafe { self.slots[i as usize].assume_init_drop() };
        self.free.push(i); // never more than n indices: never reallocates
    }
}

// ---------------------------------------------------------------- 5.3: the fixed-capacity Vec, the symbol

/// A push that refuses, rather than grows, when the Vec is full.
pub fn try_push<T>(v: &mut Vec<T>, x: T) -> Result<(), T> {
    if v.len() == v.capacity() {
        return Err(x); // the caller decides: reject, count, raise an alarm
    }
    v.push(x); // within capacity: a store and a length increment
    Ok(())
}

/// A venue symbol in 16 bytes, zero-padded: Copy, comparable, no heap.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Symbol([u8; 16]);

impl Symbol {
    pub fn new(s: &str) -> Option<Symbol> {
        let b = s.as_bytes();
        let mut buf = [0u8; 16];
        buf.get_mut(..b.len())?.copy_from_slice(b); // None if longer than 16
        Some(Symbol(buf))
    }
}

// ---------------------------------------------------------------- 5.4: finding by key

pub type InstrumentId = u32; // 0..N, assigned at startup

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Position { pub instrument: InstrumentId, _pad: u32, pub qty: i64, pub avg_price: i64, pub realised: i64 }

/// Startup only: venue symbol to dense id, and back.
pub struct Interner { ids: HashMap<String, InstrumentId>, names: Vec<String> }

impl Interner {
    pub fn new() -> Self { Interner { ids: HashMap::new(), names: Vec::new() } }
    /// The id for `name`, the next unused one the first time it is seen.
    pub fn intern(&mut self, name: &str) -> InstrumentId {
        if let Some(&id) = self.ids.get(name) { return id; }
        let id = self.names.len() as InstrumentId;
        self.ids.insert(name.to_owned(), id);
        self.names.push(name.to_owned());
        id
    }
    pub fn id(&self, name: &str) -> Option<InstrumentId> { self.ids.get(name).copied() }
    pub fn name(&self, id: InstrumentId) -> &str { &self.names[id as usize] }
    pub fn len(&self) -> usize { self.names.len() }
}

impl Interner {
    pub fn is_empty(&self) -> bool { self.names.is_empty() }
}

impl Default for Interner {
    fn default() -> Self { Self::new() }
}

/// The hot path's lookup: the id is the index. Built once, for n instruments.
pub struct Positions { by_id: Vec<Position> }

impl Positions {
    pub fn new(n: usize) -> Self {
        Positions { by_id: (0..n as u32).map(|id| Position { instrument: id, ..Default::default() }).collect() }
    }
    pub fn get_mut(&mut self, id: InstrumentId) -> &mut Position { &mut self.by_id[id as usize] }
    pub fn qty(&self, id: InstrumentId) -> i64 { self.by_id[id as usize].qty }
}

#[inline(never)] // its own symbol, so that cargo asm can find it
pub fn qty_of(p: &Positions, id: InstrumentId) -> i64 { p.qty(id) }

/// FxHash, hand-written as the lab asks: for an integer, an add, a multiply by
/// a large odd constant, and a rotate when the hash is read. No key, so anyone
/// who knows the formula can make keys collide: for ids the program assigned.
#[derive(Default, Clone, Copy)]
pub struct FxHasher(u64);

const FX_K: u64 = 0xf135_7aea_2e62_a9c5; // large and odd

impl Hasher for FxHasher {
    #[inline]
    fn write_u32(&mut self, i: u32) { self.0 = self.0.wrapping_add(i as u64).wrapping_mul(FX_K); }
    #[inline]
    fn write_u64(&mut self, i: u64) { self.0 = self.0.wrapping_add(i).wrapping_mul(FX_K); }
    #[inline]
    fn write(&mut self, bytes: &[u8]) {                  // other keys: 8 bytes at a time
        for c in bytes.chunks(8) {
            let mut w = [0u8; 8];
            w[..c.len()].copy_from_slice(c);
            self.write_u64(u64::from_le_bytes(w));
        }
    }
    #[inline]
    fn finish(&self) -> u64 { self.0.rotate_left(26) }  // the multiply's best bits down to the bucket index
}

/// `HashMap` with FxHash: the hasher is a type parameter of the map.
pub type FxBuild = std::hash::BuildHasherDefault<FxHasher>;

// ---------------------------------------------------------------- 5.5: bitmaps

/// Bit i: instrument i has an order sent and not yet acknowledged.
#[derive(Default)]
pub struct Pending(u64);

impl Pending {
    pub fn set(&mut self, id: InstrumentId) { self.0 |= 1u64 << id; }      // on send
    pub fn clear(&mut self, id: InstrumentId) { self.0 &= !(1u64 << id); } // on ack
    pub fn is_set(&self, id: InstrumentId) -> bool { (self.0 >> id) & 1 == 1 }
    pub fn count(&self) -> u32 { self.0.count_ones() }

    pub fn for_each(&self, mut f: impl FnMut(InstrumentId)) {
        let mut w = self.0;                        // a copy
        while w != 0 {
            f(w.trailing_zeros());                 // the index of the lowest set bit
            w &= w - 1;                            // clear that bit
        }
    }
}

/// Exercise 5's bit-scan over one 64-bit bitmap: mask off bits 0 to `i`, then
/// `trailing_zeros`. The lowest set bit above `i` (below 64), or 64 for none.
/// No loop and no branch: the same instructions at any distance.
#[inline(never)] // its own symbol: `cargo asm --lib bitscan_above` should show tzcnt or bsf
pub fn bitscan_above(bits: u64, i: u32) -> u32 {
    (bits & ((u64::MAX << i) << 1)).trailing_zeros()   // shifted twice: i = 63 never shifts by 64
}

/// Exercise 5's alternative: test the flags above `i` in turn. The first set
/// one, or 64. About 1 ns per 8 flags, and a branch on each.
#[inline(never)] // its own symbol: a cmp and a jne per flag, no SIMD
pub fn flagscan_above(flags: &[bool; 64], i: usize) -> usize {
    for (j, &f) in flags.iter().enumerate().skip(i + 1) {
        if f { return j; }
    }
    64
}

// ---------------------------------------------------------------- 5.8: bounds checks

pub struct Strategy { positions: Positions, n: usize, exposure: i64 } // n: live ids, 64

impl Strategy {
    #[inline(never)] // while looking: rustc emits so small a function only where it is called
    #[allow(clippy::needless_range_loop)] // the indexed loop is the point: its check is on every pass
    pub fn exposure_indexed(&mut self) {
        let p = &self.positions.by_id;
        self.exposure = 0;
        for i in 0..self.n {
            self.exposure += p[i].qty * p[i].avg_price;
        }
    }
    #[inline(never)]
    pub fn exposure_iter(&mut self) {
        let p = &self.positions.by_id[..self.n];
        self.exposure = p.iter().map(|x| x.qty * x.avg_price).sum();
    }
}

impl Strategy {
    /// The first `n` of `positions` are live.
    pub fn new(positions: Positions, n: usize) -> Self { Strategy { positions, n, exposure: 0 } }
    pub fn exposure(&self) -> i64 { self.exposure }
    pub fn positions_mut(&mut self) -> &mut Positions { &mut self.positions }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slow, obvious answer the bit tricks must agree with.
    fn scan(bits: &[u64], i: usize, up: bool) -> Option<usize> {
        let set = |j: usize| bits[j / 64] >> (j % 64) & 1 == 1;
        if up { (i + 1..bits.len() * 64).find(|&j| set(j)) } else { (0..i).rev().find(|&j| set(j)) }
    }

    #[test]
    fn bit_helpers_agree_with_a_scan() {
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut bits = vec![0u64; 8];
        for round in 0..200 {
            for w in bits.iter_mut() {
                x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                *w = if round % 3 == 0 { x & (x >> 5) & (x >> 11) } else { x & (x >> 3) };
            }
            for i in 0..bits.len() * 64 {
                assert_eq!(next_set_bit_above(&bits, i), scan(&bits, i, true), "above {i}");
                assert_eq!(next_set_bit_below(&bits, i), scan(&bits, i, false), "below {i}");
            }
        }
    }

    #[test]
    fn bit_helpers_at_the_edges() {
        let bits = [1u64 << 63, 1];
        assert_eq!(next_set_bit_above(&bits, 63), Some(64));
        assert_eq!(next_set_bit_below(&bits, 64), Some(63));
        assert_eq!(next_set_bit_above(&bits, 64), None);
        assert_eq!(next_set_bit_below(&bits, 63), None);
    }

    #[test]
    fn pool_reuses_slots_and_never_grows() {
        let mut pool: Pool<u64> = Pool::with_capacity(2);
        let a = pool.alloc(1).unwrap();
        let b = pool.alloc(2).unwrap();
        assert!(pool.alloc(3).is_none());
        assert_eq!((*pool.get(a), *pool.get(b)), (1, 2));
        pool.free(a);
        assert_eq!(pool.alloc(4), Some(a));
    }

    #[test]
    #[should_panic(expected = "freed twice")]
    fn pool_refuses_a_double_free() {
        let mut pool: Pool<u64> = Pool::with_capacity(1);
        let a = pool.alloc(1).unwrap();
        pool.free(a);
        pool.free(a);
    }

    #[test]
    fn try_push_refuses_at_capacity() {
        let mut v = Vec::with_capacity(2);
        assert_eq!((try_push(&mut v, 1), try_push(&mut v, 2)), (Ok(()), Ok(())));
        assert_eq!(try_push(&mut v, 3), Err(3));
        assert_eq!(v.capacity(), 2);
    }

    #[test]
    fn symbol_holds_sixteen_bytes_or_refuses() {
        assert_eq!(Symbol::new("BTC-USDT-PERP"), Some(Symbol(*b"BTC-USDT-PERP\0\0\0")));
        assert_eq!(Symbol::new("ABCDEFGHIJKLMNOPQ"), None);
    }

    #[test]
    fn interner_gives_dense_ids_and_names_back() {
        let mut names = Interner::new();
        for i in 0..42 { names.intern(&format!("SYM-{i}")); }
        assert_eq!(names.intern("BTC-USDT-PERP"), 42);
        assert_eq!(names.intern("BTC-USDT-PERP"), 42);           // seen: the same id
        assert_eq!((names.id("BTC-USDT-PERP"), names.id("nope")), (Some(42), None));
        assert_eq!((names.name(42), names.len()), ("BTC-USDT-PERP", 43));
    }

    #[test]
    fn positions_index_by_id_and_keep_repr_c_offsets() {
        let mut p = Positions::new(64);
        p.get_mut(42).qty = 7;
        assert_eq!((p.qty(42), qty_of(&p, 42), p.qty(41)), (7, 7, 0));
        assert_eq!(size_of::<Position>(), 32);
        assert_eq!(std::mem::offset_of!(Position, qty), 8);     // the `+ 8` in the listing
    }

    #[test]
    fn fx_map_finds_what_it_stored() {
        let mut m: HashMap<u32, i64, FxBuild> = HashMap::default();
        for id in 0..100_000u32 { m.insert(id, id as i64 * 3); }
        assert!((0..100_000u32).all(|id| m[&id] == id as i64 * 3));
        let h = |x: u32| { let mut f = FxHasher::default(); f.write_u32(x); f.finish() };
        assert_ne!(h(41) & 127, h(42) & 127, "neighbours should not share a bucket of 128");
    }

    #[test]
    fn pending_visits_only_the_set_bits() {
        let mut p = Pending::default();
        for id in [61, 5, 42] { p.set(id); }
        p.clear(5);
        let mut seen = Vec::new();
        p.for_each(|id| seen.push(id));
        assert_eq!((seen, p.count(), p.is_set(42), p.is_set(5)), (vec![42, 61], 2, true, false));
    }

    #[test]
    fn bitscan_and_flagscan_agree_at_every_place() {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..2_000 {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            let bits = x & (x >> 9);
            let flags: [bool; 64] = std::array::from_fn(|j| bits >> j & 1 == 1);
            for i in 0..64 {
                assert_eq!(bitscan_above(bits, i as u32) as usize, flagscan_above(&flags, i), "i {i} bits {bits:#x}");
            }
        }
        assert_eq!(bitscan_above(1 << 63, 63), 64);                // the top bit masks itself off
    }

    #[test]
    fn the_two_exposure_loops_agree() {
        let mut p = Positions::new(64);
        for id in 0..64 { let x = p.get_mut(id); x.qty = id as i64 - 30; x.avg_price = 6_812_345 + id as i64; }
        let mut s = Strategy::new(p, 64);
        s.exposure_indexed();
        let a = s.exposure();
        s.exposure_iter();
        assert_eq!(a, s.exposure());
        assert_ne!(a, 0);
    }
}

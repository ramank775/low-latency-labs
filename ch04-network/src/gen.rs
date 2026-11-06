//! Seeded depth updates for the parse and replay labs: prices that walk,
//! quantities whose digit counts change, sides that flip, consecutive sequence
//! numbers, as a feed's do. The same seed gives the same bytes.

use crate::Fields;

/// splitmix64: small, fast, and the same everywhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng { Rng(seed) }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in 0..n.
    pub fn below(&mut self, n: u64) -> u64 { self.next_u64() % n }
    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 { (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 }
}

/// The chapter's instrument, by number and by name.
pub const INSTRUMENT: u32 = 42;
pub const INSTRUMENT_NAME: &str = "BTC-USDT-PERP";

/// `n` updates starting from the chapter's example: seq 1,873,465,902 at
/// 68,123.45, 512 lots, the venue's clock at 1,792,046,400,123,456,789.
pub fn updates(n: usize, seed: u64) -> Vec<Fields> {
    let mut r = Rng::new(seed);
    let mut price = 6_812_345i64;
    let mut ts = 1_792_046_400_123_456_789u64;
    (0..n).map(|k| {
        if k > 0 {
            price += r.below(21) as i64 - 10;                     // a walk of up to 10 ticks
            ts += 100_000 + r.below(800_000);                     // 100–900 µs apart
        }
        let qty = match r.below(10) {
            0 => 10_000 + r.below(10_000) as i64,                 // a digit longer: "12.34500000"
            1..=3 => 1_000 + r.below(9_000) as i64,               // "1.23400000"
            _ => 1 + r.below(999) as i64,                         // "0.51200000"
        };
        let qty = if k == 0 { 512 } else { qty };
        Fields { seq: 1_873_465_902 + k as u64, ts_ns: ts, price, qty, side: (r.below(2)) as u8 }
    }).collect()
}

/// The same updates written twice, each set into one buffer allocated once,
/// with where each message starts and ends, so a timed loop does no framing.
pub struct Messages {
    pub fields: Vec<Fields>,
    pub bin: Vec<u8>,
    pub json: Vec<u8>,
    pub bin_at: Vec<(usize, usize)>,
    pub json_at: Vec<(usize, usize)>,
}

impl Messages {
    pub fn new(n: usize, seed: u64) -> Messages {
        let fields = updates(n, seed);
        let mut bin = Vec::with_capacity(48 * n);
        let mut json = Vec::with_capacity(220 * n);
        let (mut bin_at, mut json_at) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for f in &fields {
            let b = bin.len();
            bin.extend_from_slice(&crate::encode(f.seq, f.price, f.qty, INSTRUMENT, f.side, f.ts_ns));
            bin_at.push((b, bin.len()));
            let j = json.len();
            crate::json::write_update(&mut json, f, INSTRUMENT_NAME);
            json_at.push((j, json.len()));
        }
        Messages { fields, bin, json, bin_at, json_at }
    }

    pub fn bin_msg(&self, i: usize) -> &[u8] { let (a, b) = self.bin_at[i]; &self.bin[a..b] }
    pub fn json_msg(&self, i: usize) -> &[u8] { let (a, b) = self.json_at[i]; &self.json[a..b] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_seed_gives_the_same_bytes_and_both_encodings_agree() {
        let a = Messages::new(256, 7);
        let b = Messages::new(256, 7);
        assert_eq!(a.json, b.json);
        assert_eq!(a.bin, b.bin);
        assert_ne!(Messages::new(256, 8).json, a.json);
        assert_eq!(a.json_msg(0).len(), 194); // the chapter's example, exactly
        let mut lens: Vec<usize> = (0..256).map(|i| a.json_msg(i).len()).collect();
        lens.sort();
        assert!(lens[0] >= 190 && lens[255] <= 200, "{lens:?}");
        for i in 0..256 {
            let mut f = Fields::default();
            assert!(crate::read_fields(a.bin_msg(i), &mut f));
            assert_eq!(f, a.fields[i]);
            assert_eq!(crate::json::borrowed(a.json_msg(i)).unwrap().fields(), Some(f));
        }
        let sides: u32 = a.fields.iter().map(|f| f.side as u32).sum();
        assert!(sides > 64 && sides < 192, "sides flip");
    }
}

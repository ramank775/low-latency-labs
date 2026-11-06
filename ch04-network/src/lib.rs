//! The networking chapter's binary depth update: 48 bytes at fixed offsets,
//! read where the receive buffer holds them, with no allocation.
//!
//! ```text
//!  bytes   what it holds
//!  0–1     len, always 48
//!  2–3     kind, 7 is a depth update
//!  4–7     instrument
//!  8–15    seq
//! 16–23    price, in ticks
//! 24–31    qty, in lots
//! 32–39    ts_ns, the venue's clock
//! 40       side, 0 bid, 1 ask; then 7 bytes of padding
//! ```
//!
//! The rest of the chapter's code blocks live one module per section, each
//! starting with the block's own `use` lines:
//!
//! | module      | section | what it holds                                                    |
//! |-------------|---------|------------------------------------------------------------------|
//! | `calls`     | 4.1     | the two sides of a TCP conversation                              |
//! | `options`   | 4.4     | TCP_NODELAY, TCP_QUICKACK, `write_vectored`, TCP_CORK, MSG_MORE  |
//! | `multicast` | 4.5     | joining a feed's group; the A/B arbiter                          |
//! | `waiting`   | 4.6     | the mio loop, the spinning loop, SO_BUSY_POLL                     |
//! | `framer`    | 4.7     | one buffer, two cursors                                          |
//! | `json`      | 4.7     | the borrowed and owned JSON structs, text to ticks               |
//! | `tls`       | 4.8     | the rustls client (feature `tls`)                                |
//! | `hot`       | 4.10    | `open_hot`, the hot socket's setup                               |
//!
//! and the labs' shared pieces: `timing` (rdtsc and histograms), `sys` (pinning,
//! /proc counters, the C-state hold), `counting` (the counting allocator),
//! `replay` (the capture file) and `gen` (the seeded depth updates).

use zerocopy::little_endian::{I64, U16, U32, U64};
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned};

pub mod calls;
pub mod counting;
pub mod framer;
pub mod gen;
pub mod hot;
pub mod json;
pub mod multicast;
pub mod options;
pub mod replay;
pub mod sys;
pub mod timing;
#[cfg(feature = "tls")]
pub mod tls;
pub mod waiting;

pub use framer::Framer;
pub use hot::open_hot;
pub use multicast::Arbiter;
pub use waiting::set_busy_poll;

#[derive(FromBytes, KnownLayout, Immutable, Unaligned)]
#[repr(C)]
pub struct Update {
    pub len: U16,        // bytes 0..2: always 48
    pub kind: U16,       // bytes 2..4: 7 is a depth update
    pub instrument: U32, // bytes 4..8
    pub seq: U64,        // bytes 8..16
    pub price: I64,      // bytes 16..24, in ticks
    pub qty: I64,        // bytes 24..32, in lots
    pub ts_ns: U64,      // bytes 32..40, the venue's clock
    pub side: u8,        // byte 40: 0 bid, 1 ask
    pub _pad: [u8; 7],   // bytes 41..48
}

const _: () = assert!(size_of::<Update>() == 48);

/// The message type a depth update carries in `kind`.
pub const DEPTH: u16 = 7;

pub fn parse(buf: &[u8]) -> Option<&Update> {
    let (u, _rest) = Update::ref_from_prefix(buf).ok()?; // length check only
    (u.kind.get() == 7).then_some(u)
}

/// One field without the overlay: the bounds check once, then a load at a fixed offset.
pub fn price(buf: &[u8]) -> Option<i64> {
    let m: &[u8; 48] = buf.get(..48)?.try_into().ok()?; // the one bounds check
    Some(i64::from_le_bytes(m[16..24].try_into().unwrap())) // none left here
}

/// The bytes a venue would send for one depth update: what `gen` writes into a replay file.
pub fn encode(seq: u64, price: i64, qty: i64, instrument: u32, side: u8, ts_ns: u64) -> [u8; 48] {
    let mut m = [0u8; 48];
    m[0..2].copy_from_slice(&48u16.to_le_bytes());
    m[2..4].copy_from_slice(&DEPTH.to_le_bytes());
    m[4..8].copy_from_slice(&instrument.to_le_bytes());
    m[8..16].copy_from_slice(&seq.to_le_bytes());
    m[16..24].copy_from_slice(&price.to_le_bytes());
    m[24..32].copy_from_slice(&qty.to_le_bytes());
    m[32..40].copy_from_slice(&ts_ns.to_le_bytes());
    m[40] = side;
    m
}

/// The five fields the strategy reads, in the order the chapter's `cargo asm`
/// listing stores them: seq at 0, ts_ns at 8, price and qty at 16 and 24, side at 32.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Fields {
    pub seq: u64,
    pub ts_ns: u64,
    pub price: i64, // ticks of 0.01
    pub qty: i64,   // lots of 0.001
    pub side: u8,   // 0 bid, 1 ask
}

/// A function that calls `parse` and copies the five fields into the strategy's
/// struct: the one whose x86-64 the chapter prints, two compares, two branches
/// and a handful of movs. Not inlined, so `objdump` or `cargo asm` can find it.
#[inline(never)]
pub fn read_fields(buf: &[u8], out: &mut Fields) -> bool {
    let Some(u) = parse(buf) else { return false };
    out.seq = u.seq.get();
    out.ts_ns = u.ts_ns.get();
    out.price = u.price.get();
    out.qty = u.qty.get();
    out.side = u.side;
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_then_parse_round_trips() {
        let m = encode(1_873_465_902, 6_812_345, 512, 42, 0, 1_761_000_000_000_000_000);
        let u = parse(&m).expect("a depth update");
        assert_eq!(u.len.get(), 48);
        assert_eq!(u.instrument.get(), 42);
        assert_eq!(u.seq.get(), 1_873_465_902);
        assert_eq!(u.price.get(), 6_812_345);
        assert_eq!(u.qty.get(), 512);
        assert_eq!(u.side, 0);
        assert_eq!(price(&m), Some(6_812_345));
    }

    #[test]
    fn the_layout_matches_the_chapters_dump() {
        // the networking chapter's hex dump of seq 1,873,465,902 at 68,123.45, 512 lots
        let m = encode(1_873_465_902, 6_812_345, 512, 42, 0, 0x18de_a0b4_cc60_4d15);
        assert_eq!(&m[0..8], &[0x30, 0x00, 0x07, 0x00, 0x2a, 0x00, 0x00, 0x00]);
        assert_eq!(&m[8..16], &[0x2e, 0xd2, 0xaa, 0x6f, 0, 0, 0, 0]);
        assert_eq!(&m[16..24], &[0xb9, 0xf2, 0x67, 0, 0, 0, 0, 0]);
        assert_eq!(&m[24..32], &[0x00, 0x02, 0, 0, 0, 0, 0, 0]);
        assert_eq!(&m[32..40], &[0x15, 0x4d, 0x60, 0xcc, 0xb4, 0xa0, 0xde, 0x18]);
    }

    #[test]
    fn short_or_other_kinds_are_refused() {
        let m = encode(1, 2, 3, 42, 1, 0);
        assert!(parse(&m[..47]).is_none());
        let mut other = m;
        other[2] = 8;
        assert!(parse(&other).is_none());
        assert!(price(&m[..10]).is_none());
    }

    #[test]
    fn read_fields_copies_what_parse_sees() {
        let m = encode(1_873_465_902, 6_812_345, 512, 42, 1, 99);
        let mut f = Fields::default();
        assert!(read_fields(&m, &mut f));
        assert_eq!(f, Fields { seq: 1_873_465_902, ts_ns: 99, price: 6_812_345, qty: 512, side: 1 });
        assert!(!read_fields(&m[..47], &mut f));
    }

    #[test]
    fn big_endian_by_mistake_reads_the_chapters_wrong_price() {
        // 4.7: "read as big-endian by mistake, is −5,047,858,982,631,243,776 ticks"
        let m = encode(1, 6_812_345, 512, 42, 0, 0);
        assert_eq!(i64::from_be_bytes(m[16..24].try_into().unwrap()), -5_047_858_982_631_243_776);
    }
}

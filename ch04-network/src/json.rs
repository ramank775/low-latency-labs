//! 4.7, why allocation is the real cost: the venue's JSON depth update read
//! three ways. `DepthMsg` borrows every string from the input and allocates
//! nothing; `DepthMsgOwned` is the convenient struct with six `String` fields,
//! six allocations a message; `serde_json::Value` makes seventeen.

use serde::Deserialize;

#[derive(Deserialize)]
pub struct DepthMsg<'a> {
    #[serde(rename = "type")]
    pub kind: &'a str,
    pub instrument: &'a str,
    #[serde(borrow)]
    pub data: Level<'a>,
}

#[derive(Deserialize)]
pub struct Level<'a> {
    pub sequence: u64,
    pub timestamp: u64,
    pub side: &'a str,
    pub price: &'a str,    // "68123.45000000": turned into ticks by the caller
    pub quantity: &'a str,
}

/// The block's last line, as a function: one frame, nothing allocated.
pub fn borrowed(frame: &[u8]) -> serde_json::Result<DepthMsg<'_>> {
    let msg: DepthMsg = serde_json::from_slice(frame)?; // no allocation
    Ok(msg)
}

/// Lab 4.4's `--json`: the same message into owned strings. Six `String`
/// fields, so six allocations and six frees a message.
#[derive(Deserialize)]
pub struct DepthMsgOwned {
    pub channel: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub instrument: String,
    pub data: LevelOwned,
}

#[derive(Deserialize)]
pub struct LevelOwned {
    pub sequence: u64,
    pub timestamp: u64,
    pub side: String,
    pub price: String,
    pub quantity: String,
}

/// Decimal places in a tick of price (0.01) and a lot of quantity (0.001).
pub const PRICE_PLACES: u32 = 2;
pub const QTY_PLACES: u32 = 3;

/// "68123.45000000" with 2 places is 6,812,345: a short loop over the digits,
/// no f64 along the way. Digits finer than a unit must be zeros; anything else
/// that is not a digit or the one point is refused.
#[inline]
pub fn units(s: &str, places: u32) -> Option<i64> {
    let mut v: i64 = 0;
    let mut frac: Option<u32> = None; // digits taken after the point
    let mut any = false;
    for &c in s.as_bytes() {
        match c {
            b'0'..=b'9' => {
                any = true;
                match frac {
                    Some(f) if f >= places => {
                        if c != b'0' { return None; } // finer than a tick
                        continue;
                    }
                    Some(f) => frac = Some(f + 1),
                    None => {}
                }
                v = v.checked_mul(10)?.checked_add((c - b'0') as i64)?;
            }
            b'.' if frac.is_none() => frac = Some(0),
            _ => return None,
        }
    }
    if !any { return None; }
    v.checked_mul(10i64.checked_pow(places - frac.unwrap_or(0))?)
}

fn side(s: &str) -> Option<u8> {
    match s { "bid" => Some(0), "ask" => Some(1), _ => None }
}

impl DepthMsg<'_> {
    /// The type check, then the five fields the strategy uses, as integers.
    #[inline]
    pub fn fields(&self) -> Option<crate::Fields> {
        if self.kind != "depth_update" { return None; }
        let d = &self.data;
        Some(crate::Fields { seq: d.sequence, ts_ns: d.timestamp, price: units(d.price, PRICE_PLACES)?,
                             qty: units(d.quantity, QTY_PLACES)?, side: side(d.side)? })
    }
}

impl DepthMsgOwned {
    #[inline]
    pub fn fields(&self) -> Option<crate::Fields> {
        if self.kind != "depth_update" { return None; }
        let d = &self.data;
        Some(crate::Fields { seq: d.sequence, ts_ns: d.timestamp, price: units(&d.price, PRICE_PLACES)?,
                             qty: units(&d.quantity, QTY_PLACES)?, side: side(&d.side)? })
    }
}

/// The same five fields read out of a `serde_json::Value`, by key.
#[inline]
pub fn value_fields(v: &serde_json::Value) -> Option<crate::Fields> {
    if v.get("type")?.as_str()? != "depth_update" { return None; }
    let d = v.get("data")?;
    Some(crate::Fields {
        seq: d.get("sequence")?.as_u64()?,
        ts_ns: d.get("timestamp")?.as_u64()?,
        price: units(d.get("price")?.as_str()?, PRICE_PLACES)?,
        qty: units(d.get("quantity")?.as_str()?, QTY_PLACES)?,
        side: side(d.get("side")?.as_str()?)?,
    })
}

/// Appends one update as the venue sends it, on one line, shaped like the
/// chapter's 194-byte example:
/// `{"channel":"book","type":"depth_update","instrument":"BTC-USDT-PERP","data":{...}}`
pub fn write_update(out: &mut Vec<u8>, f: &crate::Fields, instrument: &str) {
    use std::io::Write;
    let (p, q) = (f.price, f.qty);
    let side = if f.side == 0 { "bid" } else { "ask" };
    // writing into a Vec cannot fail
    let _ = write!(out,
        "{{\"channel\":\"book\",\"type\":\"depth_update\",\"instrument\":\"{instrument}\",\
         \"data\":{{\"sequence\":{},\"timestamp\":{},\"side\":\"{side}\",\
         \"price\":\"{}.{:02}000000\",\"quantity\":\"{}.{:03}00000\"}}}}",
        f.seq, f.ts_ns, p / 100, p % 100, q / 1000, q % 1000);
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = concat!(
        r#"{"channel":"book","type":"depth_update","instrument":"BTC-USDT-PERP","#,
        r#""data":{"sequence":1873465902,"timestamp":1792046400123456789,"#,
        r#""side":"bid","price":"68123.45000000","quantity":"0.51200000"}}"#);

    fn example_fields() -> crate::Fields {
        crate::Fields { seq: 1_873_465_902, ts_ns: 1_792_046_400_123_456_789, price: 6_812_345, qty: 512, side: 0 }
    }

    #[test]
    fn the_chapters_example_is_194_bytes_and_reads_the_same_three_ways() {
        assert_eq!(EXAMPLE.len(), 194);
        let want = example_fields();
        assert_eq!(borrowed(EXAMPLE.as_bytes()).unwrap().fields(), Some(want));
        let owned: DepthMsgOwned = serde_json::from_slice(EXAMPLE.as_bytes()).unwrap();
        assert_eq!(owned.channel, "book");
        assert_eq!(owned.fields(), Some(want));
        let v: serde_json::Value = serde_json::from_slice(EXAMPLE.as_bytes()).unwrap();
        assert_eq!(value_fields(&v), Some(want));
    }

    #[test]
    fn write_update_reproduces_the_example_byte_for_byte() {
        let mut out = Vec::new();
        write_update(&mut out, &example_fields(), "BTC-USDT-PERP");
        assert_eq!(std::str::from_utf8(&out).unwrap(), EXAMPLE);
    }

    #[test]
    fn text_to_ticks() {
        assert_eq!(units("68123.45000000", 2), Some(6_812_345));
        assert_eq!(units("0.51200000", 3), Some(512));
        assert_eq!(units("12.5", 3), Some(12_500));
        assert_eq!(units("7", 2), Some(700));
        assert_eq!(units("68123.45100000", 2), None); // finer than a tick
        assert_eq!(units("1.2.3", 2), None);
        assert_eq!(units("", 2), None);
        assert_eq!(units("-1", 2), None);
    }

    #[test]
    fn an_escaped_string_cannot_be_borrowed() {
        // 4.7: "serde_json then returns an error instead of quietly allocating"
        let esc = EXAMPLE.replace("BTC-USDT-PERP", "BTC\\u002dUSDT-PERP");
        assert!(borrowed(esc.as_bytes()).is_err());
    }
}

//! Lab 4.5's capture file: records, each a timestamp, a length and a payload.
//!
//! ```text
//!  bytes      what it holds
//!  0–7        ts_ns, little-endian: the capture time, any epoch
//!  8–11       len, little-endian u32
//!  12..12+len the payload: one WebSocket message after the decrypt
//! ```
//!
//! A feed client writes one with `write_record` as each frame's recv returns
//! (the simpler of the chapter's two ways); `import_hex` turns tshark's
//! "time payload-hex" lines into the same thing (the way that keeps the wire's
//! timing); `synthetic` makes a stand-in where there is no venue to capture.

use std::io::{self, BufRead, Write};

use crate::gen::{Messages, Rng};

/// Appends one record.
pub fn write_record(w: &mut impl Write, ts_ns: u64, payload: &[u8]) -> io::Result<()> {
    let len = u32::try_from(payload.len()).map_err(|_| io::Error::other("payload over 4 GB"))?;
    w.write_all(&ts_ns.to_le_bytes())?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(payload)
}

/// A whole capture, in memory before anything is timed: the file's bytes and
/// where each payload lies in them.
pub struct Capture {
    pub bytes: Vec<u8>,
    pub frames: Vec<Frame>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub ts_ns: u64,
    pub at: usize,
    pub len: usize,
}

impl Capture {
    pub fn load(path: &std::path::Path) -> io::Result<Capture> {
        Capture::from_bytes(std::fs::read(path)?)
    }

    pub fn from_bytes(bytes: Vec<u8>) -> io::Result<Capture> {
        let bad = |what: &str, at: usize| io::Error::new(io::ErrorKind::InvalidData, format!("{what} at byte {at}"));
        let mut frames = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let head = bytes.get(at..at + 12).ok_or_else(|| bad("a cut record header", at))?;
            let ts_ns = u64::from_le_bytes(head[..8].try_into().unwrap());
            let len = u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize;
            if at + 12 + len > bytes.len() { return Err(bad("a payload past the end of the file", at)); }
            if let Some(prev) = frames.last().map(|f: &Frame| f.ts_ns) {
                if ts_ns < prev { return Err(bad("a timestamp earlier than the one before", at)); }
            }
            frames.push(Frame { ts_ns, at: at + 12, len });
            at += 12 + len;
        }
        Ok(Capture { bytes, frames })
    }

    pub fn payload(&self, f: &Frame) -> &[u8] { &self.bytes[f.at..f.at + f.len] }

    /// Each frame's place in its clump, counting from 1: frames with the same
    /// timestamp were recorded together, because one recv returned them.
    pub fn places(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(self.frames.len());
        let mut place = 0u32;
        for (i, f) in self.frames.iter().enumerate() {
            place = if i > 0 && f.ts_ns == self.frames[i - 1].ts_ns { place + 1 } else { 1 };
            out.push(place);
        }
        out
    }
}

/// A stand-in for a minute of a venue's feed: `n` updates at `rate` a second,
/// most arriving alone, some in clumps of 2–14 that one recv would return at once.
pub fn synthetic(n: usize, rate: f64, seed: u64, json: bool) -> io::Result<Vec<u8>> {
    let m = Messages::new(n, seed);
    let mut r = Rng::new(seed ^ 0x5eed);
    let mut out = Vec::with_capacity(n * if json { 210 } else { 64 });
    let mean_clump = 0.97 + 0.03 * 8.0;                    // 97% of arrivals alone, 3% a clump of 2–14
    let gap_ns = 1e9 / rate * mean_clump;                  // so the message rate is `rate`
    let mut ts = 1_792_046_400_000_000_000u64;
    let mut i = 0;
    while i < n {
        ts += (-(1.0 - r.unit()).ln() * gap_ns) as u64;    // exponential gaps: Poisson arrivals
        let clump = if r.unit() < 0.97 { 1 } else { 2 + (r.below(5) + r.below(5) + r.below(5)) as usize };
        for _ in 0..clump.min(n - i) {
            let p = if json { m.json_msg(i) } else { m.bin_msg(i) };
            write_record(&mut out, ts, p)?;
            i += 1;
        }
    }
    Ok(out)
}

/// tshark's `-T fields -e frame.time_epoch -e <payload field>` output as
/// records: each line a time in seconds and one or more payloads in hex, comma
/// separated when one packet carried several frames (they share its time).
pub fn import_hex(input: impl BufRead, out: &mut impl Write) -> io::Result<usize> {
    let mut n = 0;
    for (k, line) in input.lines().enumerate() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() { continue; }
        let bad = |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("line {}: {what}", k + 1));
        let (t, hex) = line.split_once(char::is_whitespace).ok_or_else(|| bad("want <time> <hex>"))?;
        let ts_ns = seconds_to_ns(t).ok_or_else(|| bad("the time is not seconds"))?;
        for h in hex.trim().split(',') {
            let h: String = h.chars().filter(|c| *c != ':').collect();
            if !h.len().is_multiple_of(2) { return Err(bad("odd number of hex digits")); }
            let bytes: Option<Vec<u8>> = (0..h.len()).step_by(2)
                .map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect();
            write_record(out, ts_ns, &bytes.ok_or_else(|| bad("not hex"))?)?;
            n += 1;
        }
    }
    Ok(n)
}

/// "1792046400.123456789" to nanoseconds, exactly: no f64 on the way.
pub fn seconds_to_ns(s: &str) -> Option<u64> {
    let (whole, frac) = s.split_once('.').unwrap_or((s, ""));
    if frac.len() > 9 || !frac.bytes().all(|c| c.is_ascii_digit()) { return None; }
    let w: u64 = whole.parse().ok()?;
    let f: u64 = if frac.is_empty() { 0 } else { frac.parse::<u64>().ok()? * 10u64.pow(9 - frac.len() as u32) };
    w.checked_mul(1_000_000_000)?.checked_add(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_clumps_are_counted() {
        let mut v = Vec::new();
        write_record(&mut v, 10, b"a").unwrap();
        write_record(&mut v, 20, b"bc").unwrap();
        write_record(&mut v, 20, b"def").unwrap();
        write_record(&mut v, 35, b"").unwrap();
        let c = Capture::from_bytes(v).unwrap();
        assert_eq!(c.frames.len(), 4);
        assert_eq!(c.payload(&c.frames[2]), b"def");
        assert_eq!(c.places(), vec![1, 1, 2, 1]);
    }

    #[test]
    fn a_cut_or_backwards_file_is_refused() {
        let mut v = Vec::new();
        write_record(&mut v, 10, b"abc").unwrap();
        assert!(Capture::from_bytes(v[..v.len() - 1].to_vec()).is_err());
        write_record(&mut v, 5, b"x").unwrap();
        assert!(Capture::from_bytes(v).is_err());
    }

    #[test]
    fn a_synthetic_minute_has_its_rate_and_its_clumps() {
        let c = Capture::from_bytes(synthetic(120_000, 2_000.0, 7, true).unwrap()).unwrap();
        assert_eq!(c.frames.len(), 120_000);
        let span = (c.frames.last().unwrap().ts_ns - c.frames[0].ts_ns) as f64 / 1e9;
        assert!(span > 50.0 && span < 70.0, "{span} s");
        let biggest = *c.places().iter().max().unwrap();
        assert!((10..=14).contains(&biggest), "{biggest}");
        assert!(crate::json::borrowed(c.payload(&c.frames[0])).is_ok());
    }

    #[test]
    fn tshark_lines_become_records() {
        let text = "1792046400.000500000\t7b7d,5b5d\n\n1792046400.001\t30:31\n";
        let mut out = Vec::new();
        assert_eq!(import_hex(text.as_bytes(), &mut out).unwrap(), 3);
        let c = Capture::from_bytes(out).unwrap();
        assert_eq!(c.payload(&c.frames[0]), b"{}");
        assert_eq!(c.payload(&c.frames[1]), b"[]");
        assert_eq!(c.payload(&c.frames[2]), b"01");
        assert_eq!(c.frames[1].ts_ns, 1_792_046_400_000_500_000);
        assert_eq!(c.frames[2].ts_ns - c.frames[1].ts_ns, 500_000);
        assert!(import_hex("1.0 zz\n".as_bytes(), &mut Vec::new()).is_err());
        assert_eq!(seconds_to_ns("2"), Some(2_000_000_000));
    }
}

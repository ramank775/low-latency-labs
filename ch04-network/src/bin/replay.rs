//! replay: Lab 4.5 (stretch), capture and replay. A capture of a feed, a file
//! of records each a timestamp, a length and a payload, is loaded into memory
//! before anything is timed, then the program's own parser runs over it:
//! back to back (--full-speed), or each frame released at its recorded offset
//! from the first (--paced), spinning on rdtsc in between.
//!
//!     replay --file <path> --full-speed | --paced  [--parser P] [--core N]
//!     replay --write-synthetic <path> [--encoding json|binary] [--messages N] [--rate R] [--seed S]
//!     replay --import-hex <tshark.txt> --out <path>

use std::hint::black_box;
use std::io::{BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::exit;
use std::time::Instant;

use ch04_network::json::{self, DepthMsgOwned};
use ch04_network::replay::{self, Capture};
use ch04_network::sys::{self, pin_or_say};
use ch04_network::timing::{self, calibrate, clock_pair_p50, commas, human, new_hist, now, ns};
use ch04_network::{read_fields, Fields};

const HELP: &str = "\
replay: Lab 4.5 (stretch), capture and replay: a captured feed loaded into memory before timing, then the
program's parser run over it, back to back or at the recorded pace.

usage:
  replay --file <path> --full-speed   frames parsed back to back: messages and megabytes a second, and
                                      p50 / p99 / max per message
  replay --file <path> --paced        each frame released at its recorded offset from the first, spinning
                                      on rdtsc; p50 / p99 / max from release to the end of its parse
  replay --write-synthetic <path>     a stand-in capture, for a box with no venue to record:
         [--encoding json|binary] [--messages N] [--rate R] [--seed S]
                                      (defaults: json, 120,000 messages, 2,000 a second: a minute of an
                                      ordinary day, most frames alone, some in clumps of 2-14; seed 7)
  replay --import-hex <txt> --out <path>   tshark's \"<time> <payload hex>[,<hex>...]\" lines as a capture

options for --file:
  --parser P   auto (default: binary for 48-byte depth updates, json-borrowed for JSON), binary, json,
               json-borrowed or json-value
  --core N     the core to pin to (default 2, the feed thread's)

The file: records of an 8-byte little-endian timestamp in ns, a 4-byte length and the payload, one per
WebSocket message after the decrypt. Capturing a real feed, two ways (see the crate's README):
  1. the client writes each frame's payload as its recv returns: ch04_network::replay::write_record
  2. SSLKEYLOGFILE=keys.log for the client (rustls: config.key_log = Arc::new(KeyLogFile::new()));
     tcpdump -i <iface> -B 65536 -w feed.pcap host <venue>, started before the client connects, for 60 s;
     tshark -r feed.pcap -o tls.keylog_file:keys.log -Y websocket -T fields -e frame.time_epoch \\
            -e websocket.payload > feed.txt      (field names vary by version: tshark -G fields | grep websocket)
     replay --import-hex feed.txt --out feed.rec
The replay leaves out the decrypt: charge each TLS record 100-300 ns fixed plus 0.5-1 ns a byte, checked
on this core with openssl speed -evp aes-128-gcm -decrypt -bytes <median record size>.";

#[derive(Clone, Copy, PartialEq)]
enum Parser { Auto, Binary, Json, JsonBorrowed, JsonValue }

fn usage(msg: &str) -> ! {
    eprintln!("replay: {msg}\n\n{HELP}");
    exit(2)
}

#[inline(always)]
fn parse_one(p: Parser, b: &[u8]) -> Option<Fields> {
    match p {
        Parser::Binary => { let mut f = Fields::default(); read_fields(b, &mut f).then_some(f) }
        Parser::Json => serde_json::from_slice::<DepthMsgOwned>(b).ok()?.fields(),
        Parser::JsonBorrowed => json::borrowed(b).ok()?.fields(),
        Parser::JsonValue => json::value_fields(&serde_json::from_slice(b).ok()?),
        Parser::Auto => unreachable!("resolved before the run"),
    }
}

fn pct(v: &mut [u64], q: f64) -> u64 {
    if v.is_empty() { return 0; }
    v.sort_unstable();
    v[((v.len() - 1) as f64 * q).round() as usize]
}

fn main() {
    let (mut file, mut full, mut paced, mut parser, mut core) = (None::<PathBuf>, false, false, Parser::Auto, Some(2usize));
    let (mut synth, mut import, mut out) = (None::<PathBuf>, None::<PathBuf>, None::<PathBuf>);
    let (mut json_enc, mut n, mut rate, mut seed) = (true, 120_000u64, 2_000.0f64, 7u64);
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut val = || it.next().unwrap_or_else(|| usage(&format!("{flag} needs a value")));
        match flag.as_str() {
            "--file" => file = Some(val().into()),
            "--full-speed" => full = true,
            "--paced" => paced = true,
            "--parser" => parser = match val().as_str() {
                "auto" => Parser::Auto, "binary" => Parser::Binary, "json" => Parser::Json,
                "json-borrowed" => Parser::JsonBorrowed, "json-value" => Parser::JsonValue,
                p => usage(&format!("--parser {p}: auto, binary, json, json-borrowed or json-value")),
            },
            "--core" => { let v = val(); core = if v == "none" { None } else { Some(v.parse().unwrap_or_else(|_| usage("--core: a core number"))) } }
            "--write-synthetic" => synth = Some(val().into()),
            "--encoding" => json_enc = match val().as_str() { "json" => true, "binary" => false, e => usage(&format!("--encoding {e}: json or binary")) },
            "--messages" => n = timing::count(&val()).unwrap_or_else(|| usage("--messages: a count")),
            "--rate" => rate = val().parse().unwrap_or_else(|_| usage("--rate: messages a second")),
            "--seed" => seed = val().parse().unwrap_or_else(|_| usage("--seed: an integer")),
            "--import-hex" => import = Some(val().into()),
            "--out" => out = Some(val().into()),
            "-h" | "--help" => { println!("{HELP}"); exit(0) }
            other => usage(&format!("unknown flag {other}")),
        }
    }

    if let Some(path) = synth {
        if n == 0 || rate <= 0.0 { usage("--messages and --rate must be positive"); }
        let bytes = replay::synthetic(n as usize, rate, seed, json_enc).unwrap_or_else(|e| usage(&format!("{e}")));
        std::fs::write(&path, &bytes).unwrap_or_else(|e| usage(&format!("{}: {e}", path.display())));
        println!("replay: wrote {} {} messages at {rate} a second (seed {seed}) to {}: {} bytes with the record headers",
                 commas(n), if json_enc { "JSON" } else { "binary" }, path.display(), commas(bytes.len() as u64));
        return;
    }
    if let Some(src) = import {
        let dst = out.unwrap_or_else(|| usage("--import-hex needs --out <path>"));
        let r = std::fs::File::open(&src).unwrap_or_else(|e| usage(&format!("{}: {e}", src.display())));
        let w = std::fs::File::create(&dst).unwrap_or_else(|e| usage(&format!("{}: {e}", dst.display())));
        let mut w = BufWriter::new(w);
        let k = replay::import_hex(BufReader::new(r), &mut w).and_then(|k| w.flush().map(|_| k))
            .unwrap_or_else(|e| usage(&format!("{}: {e}", src.display())));
        println!("replay: {} frames from {} written to {}", commas(k as u64), src.display(), dst.display());
        return;
    }

    let path = file.unwrap_or_else(|| usage("give --file <path>, --write-synthetic <path> or --import-hex <txt>"));
    if full == paced { usage("give one of --full-speed or --paced"); }
    if full {
        timing::predict(&["JSON: 1-3 us for 194 bytes in a running handler is 5-15 ns a byte; this loop is as warm as Lab 4.4's,",
                          "so expect that lab's rate per byte. The binary stream: a hundred times faster, 10-20 ns a message.",
                          "A minute of an ordinary day, about 120,000 messages at 2,000 a second, should replay in well under a second."]);
    } else {
        timing::predict(&["At the recorded pace most frames arrive alone and the p50 is one parse. The tail comes from clumps:",
                          "the k-th of a clump waits behind k - 1 parses, so the last of a clump of ten finishes 10-30 us after",
                          "its release (JSON). Predict the p99 as the 99th percentile of the places times the full-speed time",
                          "(worked out for this file below, before the paced run)."]);
    }
    let cap = Capture::load(&path).unwrap_or_else(|e| usage(&format!("{}: {e}", path.display())));
    if cap.frames.is_empty() { usage("the capture holds no frames"); }
    if parser == Parser::Auto {
        let first = cap.payload(&cap.frames[0]);
        parser = if first.first() == Some(&b'{') { Parser::JsonBorrowed } else { Parser::Binary };
    }
    let pname = match parser { Parser::Binary => "binary", Parser::Json => "json", Parser::JsonBorrowed => "json-borrowed",
                               Parser::JsonValue => "json-value", Parser::Auto => unreachable!() };

    // what the capture holds: read the distribution of sizes first
    let mut sizes: Vec<u64> = cap.frames.iter().map(|f| f.len as u64).collect();
    let bytes: u64 = sizes.iter().sum();
    let places = cap.places();
    let mut pl: Vec<u64> = places.iter().map(|&p| p as u64).collect();
    let span_ns = cap.frames.last().unwrap().ts_ns - cap.frames[0].ts_ns;
    let (s50, s99, smax) = (pct(&mut sizes, 0.5), pct(&mut sizes, 0.99), *sizes.last().unwrap());
    let (p99_place, biggest) = (pct(&mut pl, 0.99), *pl.last().unwrap());

    let tpn = calibrate();
    pin_or_say(core, "replay");
    println!("replay {}: {} frames, {} bytes of payload over {:.1} s ({:.0} a second); sizes p50 {s50} p99 {s99} max {smax} bytes; \
              largest clump {biggest}, p99 place in clump {p99_place}; parser {pname}; {}",
             path.display(), commas(cap.frames.len() as u64), commas(bytes), span_ns as f64 / 1e9,
             cap.frames.len() as f64 / (span_ns as f64 / 1e9).max(1e-9), sys::describe_box(tpn));

    // a pass at full speed: the rate, and the warm-up for whatever follows
    let full_pass = || {
        let mut fails = 0u64;
        let t = Instant::now();
        for f in &cap.frames {
            if black_box(parse_one(parser, cap.payload(f))).is_none() { fails += 1; }
        }
        (t.elapsed(), fails)
    };

    if full {
        let (_, _) = full_pass(); // warm-up
        let (t, fails) = full_pass();
        let secs = t.as_secs_f64();
        println!("replay --full-speed: {} messages in {}: {} messages a second, {:.1} MB a second, {:.1} ns a message, \
                  {:.2} ns a byte; {fails} frames the parser refused",
                 commas(cap.frames.len() as u64), human(t.as_nanos() as u64),
                 commas((cap.frames.len() as f64 / secs) as u64), bytes as f64 / secs / 1e6,
                 t.as_nanos() as f64 / cap.frames.len() as f64, t.as_nanos() as f64 / bytes as f64);
        // and each message's own time, the clock's share taken off
        let clock = clock_pair_p50(10_000);
        let mut h = new_hist();
        for f in &cap.frames {
            let t0 = now();
            black_box(parse_one(parser, cap.payload(f)));
            let t1 = now();
            h.saturating_record(ns((t1 - t0).saturating_sub(clock), tpn));
        }
        timing::report("replay --full-speed, per message", &h);
    } else {
        let (t, _) = full_pass(); // warm-up, and the full-speed time per message for the prediction
        let per = t.as_nanos() as f64 / cap.frames.len() as f64;
        println!("  the prediction for this file: {per:.0} ns a message at full speed and a p99 place in clump of {p99_place}, \
                  so p50 about {per:.0} ns (one parse), p99 about {p99_place} x {per:.0} = {:.0} ns; the largest clump, \
                  {biggest}, finishes about {:.1} us after its release",
                 p99_place as f64 * per, biggest as f64 * per / 1e3);
        let mut h = new_hist();
        let mut fails = 0u64;
        let start = now() + (1_000_000.0 * tpn) as u64; // 1 ms from now
        let base = cap.frames[0].ts_ns;
        for f in &cap.frames {
            let release = start + ((f.ts_ns - base) as f64 * tpn) as u64;
            while now() < release { std::hint::spin_loop(); } // a sleep would stretch every short gap by the timer slack
            if black_box(parse_one(parser, cap.payload(f))).is_none() { fails += 1; }
            let done = now();
            h.saturating_record(ns(done.saturating_sub(release), tpn));
        }
        timing::report("replay --paced, release to end of parse", &h);
        println!("  {fails} frames the parser refused; a parse after a quiet spell runs colder than the full-speed loop's, \
                  its caches and predictor shared with whatever ran meanwhile");
    }
    // one WebSocket frame in one TLS 1.3 record: a 2- or 4-byte frame header, the type byte, then 5 + 16 around it
    let ws = if s50 < 126 { 2 } else { 4 };
    let sealed = s50 + ws + 1;
    println!("  not replayed: the decrypt. One update a record is a {}-byte record, {sealed} bytes sealed: 100-300 ns fixed \
              + 0.5-1 ns a byte = {}-{} ns a record (the chapter: 300-500 ns for about 200 bytes); \
              check: openssl speed -evp aes-128-gcm -decrypt -bytes {sealed}",
             sealed + 21, 100 + sealed / 2, 300 + sealed);
}

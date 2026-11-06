// 5.8's benchmark honesty and the two exposure loops:
//   cargo run --release -p ch05-structures --example exposure
//   perf stat -e branch-misses,instructions -- taskset -c 3 target/release/examples/exposure
// Calls each loop ten million times; the chapter predicts about 650 instructions
// a call for the indexed loop against about 280 for the iterator, branch misses
// near zero for both.
use ch05_structures::lab::{fmt_ns, Clock};
use ch05_structures::{Positions, Strategy};
use std::hint::black_box;
use std::time::Instant;

/// The sum the strategy computes after a batch, as a free function.
fn exposure(p: &Positions) -> i64 { (0..64).map(|id| p.qty(id) * 3).sum() }

fn main() {
    let clock = Clock::new();
    let mut positions = Positions::new(64);
    for id in 0..64 { let x = positions.get_mut(id); x.qty = id as i64 - 30; x.avg_price = 6_812_345 + id as i64; }

    let t = Instant::now();
    for _ in 0..1_000_000 { exposure(&positions); }                       // deleted
    println!("unused result, 1,000,000 calls: {:?} (the loop was deleted)", t.elapsed());
    let t = Instant::now();
    for _ in 0..1_000_000 { black_box(exposure(black_box(&positions))); } // measured
    println!("black_box on both ends:          {:?}", t.elapsed());

    let mut s = Strategy::new(positions, 64);
    let calls = 10_000_000u64;
    let t = Instant::now();
    for _ in 0..calls { black_box(&mut s).exposure_indexed(); }
    let indexed = t.elapsed().as_nanos() as f64 / calls as f64;
    let a = s.exposure();
    let t = Instant::now();
    for _ in 0..calls { black_box(&mut s).exposure_iter(); }
    let iter = t.elapsed().as_nanos() as f64 / calls as f64;
    assert_eq!(a, s.exposure());
    println!("64 positions, {calls} calls each: exposure_indexed {} ns a call, exposure_iter {} ns ({})",
             fmt_ns(indexed), fmt_ns(iter), clock.describe());
}

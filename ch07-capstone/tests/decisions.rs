// tests/decisions.rs: a swap changes the time and nothing else. Two runs need
// not close their batches at the same updates, but wherever both decided at
// the same sequence number their decisions must match exactly.
use ch07_capstone::pipeline::{self, BookKind, Config};
use ch07_capstone::queue::Queue;
use ch07_capstone::{calibrate, synth, Decision, Replay};
use std::collections::HashMap;

fn decisions(r: &Replay, book: BookKind, queue: Queue, tpn: f64) -> Vec<Decision> {
    let cfg = Config { paced: false, book, queue, records: usize::MAX, warmup: 1_000, stall_every_ms: 0,
                       feed_core: None, strategy_core: None, gateway_core: None, tpn };
    pipeline::replay(r, &cfg).report.log.v
}

#[test]
fn every_swap_decides_the_same_wherever_both_decided() {
    let tpn = calibrate();
    let r = Replay::from_bytes(synth::generate(&synth::Params { records: 100_000, seed: 11, ..Default::default() }));
    let base: HashMap<u64, Decision> = decisions(&r, BookKind::Array, Queue::Spsc, tpn)
        .into_iter().map(|d| (d.seq, d)).collect();
    for (book, queue) in [(BookKind::Tree, Queue::Spsc), (BookKind::Vec, Queue::Spsc),
                          (BookKind::Array, Queue::Mutex), (BookKind::Array, Queue::Mpsc)] {
        let other = decisions(&r, book, queue, tpn);
        assert_eq!(other.last().map(|d| d.seq), Some(100_000), "{book:?}/{queue:?} saw the whole file");
        let mut joined = 0;
        for d in &other {
            if let Some(b) = base.get(&d.seq) {
                assert_eq!((d.top, d.want), (b.top, b.want), "{book:?}/{queue:?} differs at seq {}", d.seq);
                joined += 1;
            }
        }
        assert!(joined > 0, "{book:?}/{queue:?}: no common decisions to compare");
    }
}

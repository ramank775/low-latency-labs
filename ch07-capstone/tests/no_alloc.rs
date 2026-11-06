// tests/no_alloc.rs, a binary of its own: the counting allocator sees every thread.
use ch05_structures::counting::Counting;
use ch07_capstone::pipeline::{self, BookKind, Config};
use ch07_capstone::queue::Queue;
use ch07_capstone::{calibrate, synth, BookState, Replay};

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The whole assembled loop, flat, in this process: feed, strategy and
/// gateway threads, the ring, the dense book and the histograms. After the
/// warm-up, not one allocation, on any thread.
#[test]
fn flat_replay_allocates_nothing_after_warmup() {
    let r = Replay::from_bytes(synth::generate(&synth::Params { records: 150_000, seed: 3, ..Default::default() }));
    let cfg = Config { paced: false, book: BookKind::Array, queue: Queue::Spsc, records: usize::MAX,
                       warmup: 20_000, stall_every_ms: 0, feed_core: None, strategy_core: None,
                       gateway_core: None, tpn: calibrate() };
    let s = pipeline::replay(&r, &cfg);
    assert_eq!(s.report.state, BookState::Live);
    assert_eq!(s.report.h.total.len(), 130_000, "every message after warm-up recorded");
    assert_eq!(s.report.h.dropped, 0);
    let (allocs, _frees) = s.allocs.expect("the counting allocator is installed and warm-up ended");
    assert_eq!(allocs, 0, "allocated in the loop after warm-up");
}

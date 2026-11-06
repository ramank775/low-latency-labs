// tests/mpsc_allocates.rs, a binary of its own: proof that the counter sees the
// hot threads, since the channel swap must fail where the ring passes.
use ch05_structures::counting::Counting;
use ch07_capstone::pipeline::{self, BookKind, Config};
use ch07_capstone::queue::Queue;
use ch07_capstone::{calibrate, synth, Replay};

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn the_channel_allocates_a_block_every_31_messages() {
    let r = Replay::from_bytes(synth::generate(&synth::Params { records: 60_000, seed: 3, ..Default::default() }));
    let cfg = Config { paced: false, book: BookKind::Array, queue: Queue::Mpsc, records: usize::MAX,
                       warmup: 20_000, stall_every_ms: 0, feed_core: None, strategy_core: None,
                       gateway_core: None, tpn: calibrate() };
    let s = pipeline::replay(&r, &cfg);
    let (allocs, _) = s.allocs.expect("counting allocator installed");
    // at most 40,000 messages after warm-up, one block per 31: up to about 1,290. The
    // main thread reads the counter up to a millisecond after warm-up ends, while a
    // flat feed runs ahead, so the count is a little lower; it must not be zero.
    assert!(allocs > 100, "only {allocs} allocations: the counter does not see the hot threads");
}

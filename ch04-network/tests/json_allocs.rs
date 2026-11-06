// tests/json_allocs.rs, a binary of its own: the counters see every thread
use ch04_network::counting::{Counting, ALLOCS, FREES};
use ch04_network::gen::Messages;
use ch04_network::json::{self, DepthMsgOwned};
use ch04_network::{read_fields, Fields};
use std::hint::black_box;
use std::sync::atomic::Ordering::Relaxed;

#[global_allocator]
static GLOBAL: Counting = Counting;

/// 4.7: "It passes with the binary parser and with the borrowed JSON struct. It
/// fails with the owned struct, by six allocations per message, and with
/// serde_json::Value, by seventeen." Counted over the 256 generated updates.
#[test]
fn allocations_per_message_are_0_0_6_and_17() {
    assert!(ch04_network::counting::installed());
    let m = Messages::new(256, 7);
    let per = |each: &dyn Fn(usize) -> Option<Fields>| -> (f64, f64) {
        let (a0, f0) = (ALLOCS.load(Relaxed), FREES.load(Relaxed));
        for i in 0..256 { assert!(black_box(each(i)).is_some()); }
        ((ALLOCS.load(Relaxed) - a0) as f64 / 256.0, (FREES.load(Relaxed) - f0) as f64 / 256.0)
    };
    let binary = per(&|i| { let mut f = Fields::default(); read_fields(m.bin_msg(i), &mut f).then_some(f) });
    let borrowed = per(&|i| json::borrowed(m.json_msg(i)).ok()?.fields());
    let owned = per(&|i| serde_json::from_slice::<DepthMsgOwned>(m.json_msg(i)).ok()?.fields());
    let value = per(&|i| json::value_fields(&serde_json::from_slice(m.json_msg(i)).ok()?));
    assert_eq!(binary, (0.0, 0.0), "binary");
    assert_eq!(borrowed, (0.0, 0.0), "borrowed JSON");
    assert_eq!(owned, (6.0, 6.0), "owned JSON: six String fields");
    assert_eq!(value, (17.0, 17.0), "serde_json::Value: two maps, nine keys, six string values");
}

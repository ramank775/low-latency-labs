// tests/jitter_no_alloc.rs, a binary of its own: the allocation counter sees every thread.
use ch02_os::counting::allocs;
use ch02_os::{above, calibrate, jitter, new_hist, now_unfenced, ToNs};

/// Lab 2.1's loop records every gap and allocates nothing, so the lab measures
/// the platform and not the allocator.
#[test]
fn jitter_loop_records_every_gap_and_allocates_nothing() {
    let tpn = calibrate();
    let ns = ToNs::new(tpn);
    let mut h = new_hist();
    let a0 = allocs();
    let backwards = jitter(&mut h, now_unfenced() + (50e6 * tpn) as u64, ns);   // 50 ms
    let a1 = allocs();
    assert_eq!(a1 - a0, 0, "allocated in the loop");
    assert!(h.len() > 100_000, "only {} gaps in 50 ms", h.len());
    assert!(h.value_at_quantile(0.5) < 1_000, "p50 {} ns: two rdtsc reads are tens of ns apart", h.value_at_quantile(0.5));
    assert!(above(&h, 1_000) < h.len() / 100);
    assert_eq!(backwards, 0, "a pinned or unpinned thread on one synchronised TSC never goes backwards");
}

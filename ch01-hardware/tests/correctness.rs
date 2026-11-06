// tests/correctness.rs: the chapter's types have the layouts it states, and
// every variant a lab times computes the same answer as the plain version.

use ch01_hardware::ring::{ring, CAPACITY};
use ch01_hardware::*;
use std::mem::{align_of, offset_of, size_of};
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicU64, Ordering};

#[test]
fn layouts_are_the_chapters() {
    assert_eq!((size_of::<OrderC>(), align_of::<OrderC>()), (40, 8));
    assert_eq!((size_of::<OrderSorted>(), align_of::<OrderSorted>()), (24, 8));
    assert_eq!((size_of::<OrderRust>(), align_of::<OrderRust>()), (24, 8), "the current compiler reorders to 24");
    assert_eq!((offset_of!(OrderC, price), offset_of!(OrderC, qty), offset_of!(OrderC, id)), (8, 20, 32));
    assert_eq!((size_of::<PriceLevel>(), align_of::<PriceLevel>()), (32, 8));
    assert_eq!(size_of::<Counters>(), 128);
    assert_eq!((offset_of!(Counters, a), offset_of!(Counters, b)), (0, 64));
    assert_eq!(size_of::<Option<u64>>(), 16);
    assert_eq!(size_of::<Option<NonZeroU64>>(), 8);
    assert_eq!(align_of::<CachePadded<AtomicU64>>(), 128, "x86-64: pairs of lines");
}

#[test]
fn adjacent_counters_share_one_line_padded_ones_do_not() {
    let adj = Adjacent::default();
    let (a, b) = (&adj.a as *const AtomicU64 as usize, &adj.b as *const AtomicU64 as usize);
    assert_eq!((a % 64, b - a, a / 64), (0, 8, b / 64), "offsets 0 and 8 of one line");
    let pad = PaddedPair::default();
    let (a, b) = (&*pad.a as *const AtomicU64 as usize, &*pad.b as *const AtomicU64 as usize);
    assert!(b - a >= 128);
    let c = Counters::default();
    let (a, b) = (&c.a.0 as *const AtomicU64 as usize, &c.b.0 as *const AtomicU64 as usize);
    assert_ne!(a / 64, b / 64);
}

#[test]
fn run_pair_counts_every_increment() {
    let adj = Adjacent::default();
    let t = run_pair(&adj.a, &adj.b, [CoreArg::None, CoreArg::None], 100_000, 7_000);
    assert_eq!((adj.a.load(Ordering::Relaxed), adj.b.load(Ordering::Relaxed)), (100_000, 100_000));
    for h in &t {
        assert_eq!(h.blocks.len(), 15);                          // 14 full blocks and one of 2,000
        assert_eq!(h.blocks.iter().map(|b| b.0).sum::<u64>(), 100_000);
        assert!(h.end >= h.start && !h.pinned.refused);
    }
}

#[test]
fn sum4_matches_sum1() {
    let v: Vec<f64> = (0..4099).map(|i| i as f64).collect();   // a remainder of 3
    assert_eq!(sum1(&v), 4099.0 * 4098.0 / 2.0);                 // whole numbers: exact either way
    assert_eq!(sum4(&v), sum1(&v));
    assert_eq!(sum4(&[]), 0.0);
}

#[test]
fn branch_free_forms_select_what_the_branch_would() {
    for cond in [false, true] {
        assert_eq!(select_px(cond, 101, 99), if cond { 101 } else { 99 });
    }
    let bytes: Vec<u8> = (0..=255).collect();
    assert_eq!(count_above_128(&bytes), 127);
    assert_eq!(apply_fill(10, Side::Bid, 3), 13);
    assert_eq!(apply_fill(10, Side::Ask, 3), 7);
    assert_eq!((best_px(5, 7), best_px(7, 5)), (7, 7));
}

#[test]
fn branch_variants_agree() {
    let data = random_bytes(1_000_003, 7);
    let plain: u64 = data.iter().filter(|&&b| b > 128).map(|&b| b as u64).sum();
    let sorted = counting_sort(&data);
    assert!(sorted.windows(2).all(|w| w[0] <= w[1]) && sorted.len() == data.len());
    assert_eq!(sum_branchy(&data), plain);
    assert_eq!(sum_branchy(&sorted), plain);
    assert_eq!(sum_branchless(&data), plain);
    assert_eq!(sum_vector(&data), plain);
    let above = data.iter().filter(|&&b| b > 128).count() as f64 / data.len() as f64;
    assert!((above - 127.0 / 256.0).abs() < 0.005, "uniform bytes: 127 of 256 values are above 128");
    assert_eq!(random_bytes(1000, 7), random_bytes(1000, 7), "same seed, same bytes");
}

#[test]
fn stride_sum_takes_every_strideth_element() {
    let mut v = Buffer::new(1 << 16, 0u64, false);
    assert_eq!(v.as_ptr() as usize % HUGE_PAGE, 0, "on a 2 MB boundary");
    for (i, x) in v.iter_mut().enumerate() { *x = i as u64; }
    for &s in STRIDES.iter() {
        let n = accesses(v.len(), s) as u64;
        assert_eq!(stride_sum(&v, s), s as u64 * n * (n - 1) / 2, "stride {s}");
        flush_touched(&v, s);
        assert_eq!(stride_sum(&v, s), s as u64 * n * (n - 1) / 2, "a flush changes nothing but timing");
    }
    assert_eq!(accesses(10, 4), 3);
}

#[test]
fn every_traversal_sums_the_same() {
    for shuffle in [false, true] {
        let order = permutation(50_000, shuffle, 7);
        let mut seen = order.clone();
        seen.sort_unstable();
        assert!(seen.iter().enumerate().all(|(i, &k)| k as usize == i), "a permutation");
        let expect = 50_000u64 * 49_999 / 2;
        let list = List::build(&order);
        assert_eq!((list_sum(&list), list.len), (expect, 50_000));
        assert_eq!(boxes_sum(&boxes(&order)), expect);
        if shuffle { assert_ne!(order[..10], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]); }
    }
    let v: Vec<u64> = (0..50_000).collect();
    assert_eq!(vec_sum(&v), 50_000u64 * 49_999 / 2);
    assert_eq!(permutation(1000, true, 7), permutation(1000, true, 7), "same seed, same order");
}

#[test]
fn a_long_list_drops_without_overflowing_the_stack() {
    let list = List::build(&permutation(2_000_000, false, 7));
    drop(list);
}

#[test]
fn ring_delivers_in_order_and_refuses_when_full() {
    let (mut tx, mut rx) = ring();
    for i in 0..CAPACITY { assert!(tx.try_push(i)); }
    assert!(!tx.try_push(CAPACITY), "full at 1,024");
    assert_eq!(rx.try_pop(), Some(0));
    assert!(tx.try_push(CAPACITY), "one slot freed");

    let (mut tx, mut rx) = ring();
    const N: u64 = 1_000_000;
    std::thread::scope(|s| {
        s.spawn(move || {
            let mut i = 0;
            while i < N { if tx.try_push(i) { i += 1; } else { std::hint::spin_loop(); } }
        });
        let mut next = 0;
        while next < N {
            if let Some(m) = rx.try_pop() { assert_eq!(m, next); next += 1; } else { std::hint::spin_loop(); }
        }
        assert_eq!(rx.try_pop(), None);
    });
}

#[test]
fn percentiles_by_nearest_rank() {
    let mut v: Vec<f64> = (1..=1000).rev().map(f64::from).collect();
    let s = Summary::of(&mut v);
    assert_eq!((s.n, s.p50, s.p99, s.p999, s.max), (1000, 500.0, 990.0, 999.0, 1000.0));
    let s = Summary::of(&mut [3.0]);
    assert_eq!((s.p50, s.p99, s.max), (3.0, 3.0, 3.0));
}

#[test]
fn cpu_lists_parse() {
    assert_eq!(parse_cpu_list("0-3,8,10-11\n"), vec![0, 1, 2, 3, 8, 10, 11]);
    assert_eq!(parse_cpu_list("5"), vec![5]);
    assert!(!allowed_cpus().is_empty());
    assert_eq!(commas(1234567), "1,234,567");
    assert_eq!(commas(12), "12");
}

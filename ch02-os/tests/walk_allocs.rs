// tests/walk_allocs.rs, a binary of its own: the allocation counter sees every thread.
use ch02_os::counting::allocs;
use ch02_os::{expected_sum, walk, walk_logged, How, Log};
use std::io;

/// Lab 2.2's claims about the heap: the walk allocates nothing, writing a
/// line allocates nothing, and format! allocates once per line.
#[test]
fn the_walk_allocates_nothing_and_format_once_per_line() {
    let a: Vec<u64> = (0..200_000).collect();
    let mut direct = Log::new(How::Write, io::sink());
    let mut formatted = Log::new(How::Format, io::sink());
    walk(&a, 1);                                      // warm-up

    let a0 = allocs();
    let s = walk(&a, 1);
    let a1 = allocs();
    assert_eq!((s, a1 - a0), (expected_sum(a.len(), 1), 0));

    let s = walk_logged(&a, 1, 0, &mut direct);
    let a2 = allocs();
    assert_eq!((s, direct.lines, a2 - a1), (expected_sum(a.len(), 1), 200, 0), "writeln! allocated");

    let s = walk_logged(&a, 1, 0, &mut formatted);
    formatted.pass(0, 1, s, 1_234_567);
    let a3 = allocs();
    assert_eq!(formatted.lines, 201);
    assert_eq!(a3 - a2, formatted.lines, "format! should allocate exactly once per line");
}

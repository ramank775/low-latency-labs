// tests/poll.rs: the chapter's two code blocks, run.
use ch02_os::counting::ALLOCS;
use ch02_os::poll;
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::thread;

struct Done;

/// "Not waiting": the spin loop handles every message, in order; the handler
/// stops it by unwinding, since the loop itself never returns.
#[test]
fn the_spin_loop_handles_every_message() {
    let (tx, mut rx) = mpsc::channel::<u64>();
    let producer = thread::spawn(move || for i in 1..=10_000u64 { tx.send(i).unwrap(); });
    let (mut sum, mut last) = (0u64, 0u64);
    let stopped = catch_unwind(AssertUnwindSafe(|| {
        poll(&mut rx, |msg| {
            assert_eq!(msg, last + 1, "out of order");
            last = msg;
            sum += msg;
            if msg == 10_000 { resume_unwind(Box::new(Done)); }   // no panic hook, no message
        })
    }));
    assert!(stopped.err().is_some_and(|e| e.is::<Done>()));
    producer.join().unwrap();
    assert_eq!(sum, 10_000 * 10_001 / 2);
}

/// "Seeing it": the counting allocator counts.
#[test]
fn the_counting_allocator_counts() {
    let before = ALLOCS.load(Relaxed);
    drop(std::hint::black_box(Box::new([0u8; 64])));
    assert!(ALLOCS.load(Relaxed) > before);
}

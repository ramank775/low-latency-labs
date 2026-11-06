//! The counting allocator: forwards every request to the system's and counts
//! it, so "allocates nothing" is a number and not a reading of the code.
//!
//! It counts only where a binary installs it, once, as its global allocator:
//!
//! ```ignore
//! use ch05_structures::counting::Counting;
//! #[global_allocator]
//! static GLOBAL: Counting = Counting;
//! ```
//!
//! The counters see every thread, so a test that reads them runs alone in a
//! binary of its own.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static FREES: AtomicU64 = AtomicU64::new(0);

pub struct Counting;

// SAFETY: every request goes unchanged to `System`, which upholds GlobalAlloc's contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREES.fetch_add(1, Ordering::Relaxed);
        System.dealloc(ptr, layout)
    }
    // realloc is the trait's default: alloc, copy, dealloc, so growth is counted too
}

/// Whether this process counts: true only when a binary installed `Counting`.
pub fn installed() -> bool {
    let before = ALLOCS.load(Ordering::Relaxed);
    drop(std::hint::black_box(Box::new(0u64)));
    ALLOCS.load(Ordering::Relaxed) != before
}

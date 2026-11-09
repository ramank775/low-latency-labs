//! The counting allocator from "The allocator in between", as the chapter
//! prints it. Being in the library, it is the global allocator of every
//! binary and test in this crate, so each of them can say how many
//! allocations its measured loop made, and Lab 2.2's "build it with the
//! counting allocator" is the ordinary build.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

pub static ALLOCS: AtomicU64 = AtomicU64::new(0);

struct Counting;

// SAFETY: every request goes unchanged to `System`, which upholds GlobalAlloc's contract.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc(layout)
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        System.dealloc(ptr, layout)
    }
    // Forwarded, so calloc's zeroed pages and realloc's in-place growth stay as they were.
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.alloc_zeroed(layout)
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        System.realloc(ptr, layout, new_size)
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// The count so far: read it after warm-up and again after the loop.
pub fn allocs() -> u64 {
    ALLOCS.load(Ordering::Relaxed)
}

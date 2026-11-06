//! Section 3.7's "Seeing it": loom finds the ABA in a model with three threads
//! and three slots, and with the tag the same model passes.
//!
//!     RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency --test freelist_loom -- --nocapture
//!
//! Popper 1 pops once. Popper 2 pops twice and pushes its first slot back. The
//! main thread then drains the list, and every slot must have exactly one
//! owner: popper 1, popper 2, or the list.

#![cfg(loom)]

mod common;

use ch03_concurrency::freelist::{FreeList, TaggedFreeList};
use common::explore;
use loom::sync::Arc;

trait Stack: Send + Sync + 'static {
    fn new(n: u32) -> Self;
    fn push(&self, i: u32);
    fn pop(&self) -> Option<u32>;
}

impl Stack for FreeList {
    fn new(n: u32) -> Self { FreeList::new(n) }
    fn push(&self, i: u32) { FreeList::push(self, i) }
    fn pop(&self) -> Option<u32> { FreeList::pop(self) }
}

impl Stack for TaggedFreeList {
    fn new(n: u32) -> Self { TaggedFreeList::new(n) }
    fn push(&self, i: u32) { TaggedFreeList::push(self, i) }
    fn pop(&self) -> Option<u32> { TaggedFreeList::pop(self) }
}

fn two_poppers_and_a_push<S: Stack>() {
    let list = Arc::new(S::new(3)); // head -> A(0) -> B(1) -> C(2)
    let l1 = list.clone();
    let popper1 = loom::thread::spawn(move || l1.pop());
    let l2 = list.clone();
    let popper2 = loom::thread::spawn(move || {
        let first = l2.pop();
        let second = l2.pop();
        if let Some(a) = first { l2.push(a); } // A goes back: head is A again
        second
    });
    let mut owners = [0u32; 3];
    for s in [popper1.join().unwrap(), popper2.join().unwrap()].into_iter().flatten() {
        owners[s as usize] += 1;
    }
    for _ in 0..6 {
        // more pops than slots, so a list that ABA has bent can still be drained
        match list.pop() {
            Some(s) => owners[s as usize] += 1,
            None => break,
        }
    }
    assert_eq!(owners, [1, 1, 1], "a slot owned twice, or lost: owners per slot {owners:?}");
}

#[test]
#[should_panic(expected = "a slot owned twice")]
fn untagged_free_list_aba() {
    explore("FreeList, untagged (expected to fail: ABA)", two_poppers_and_a_push::<FreeList>);
}

#[test]
fn tagged_free_list_no_aba() {
    explore("TaggedFreeList", two_poppers_and_a_push::<TaggedFreeList>);
}

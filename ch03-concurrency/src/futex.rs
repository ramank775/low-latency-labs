//! Section 3.6: what a mutex is made of. The standard mutex's three states and
//! its `lock` and `unlock`, simplified as the chapter prints them, over the two
//! `futex` operations it names. Linux only.

use std::sync::atomic::{AtomicU32, Ordering::{Acquire, Relaxed, Release}};

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;    // held, nobody asleep
const CONTENDED: u32 = 2; // held, someone may be asleep

pub fn lock(state: &AtomicU32) {
    if state.compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed).is_ok() {
        return; // fast path: one lock cmpxchg, no system call
    }
    for _ in 0..100 {
        // spin briefly in case the holder is about to release
        if state.load(Relaxed) != LOCKED { break; }
        std::hint::spin_loop();
    }
    if state.compare_exchange(UNLOCKED, LOCKED, Acquire, Relaxed).is_ok() {
        return; // freed during the spin: take it without marking it contended
    }
    while state.swap(CONTENDED, Acquire) != UNLOCKED {
        futex_wait(state, CONTENDED); // system call: sleep while the word is 2
    }
}

pub fn unlock(state: &AtomicU32) {
    if state.swap(UNLOCKED, Release) == CONTENDED {
        futex_wake(state, 1); // system call: wake one sleeper
    }
}

/// `FUTEX_WAIT(addr, expected)`: sleep on `state` if it still equals `expected`.
/// The check and the sleep are one step against wakes, so no wake is lost.
/// It returns at once if the word has changed, and may return spuriously;
/// `lock` loops, so neither matters.
pub fn futex_wait(state: &AtomicU32, expected: u32) {
    // SAFETY: the address is a live, aligned u32 for the whole call, and a null timeout waits until woken.
    unsafe {
        libc::syscall(libc::SYS_futex, state.as_ptr(), libc::FUTEX_WAIT | libc::FUTEX_PRIVATE_FLAG,
                      expected, std::ptr::null::<libc::timespec>());
    }
}

/// `FUTEX_WAKE(addr, n)`: wake up to `n` threads asleep on `state`.
pub fn futex_wake(state: &AtomicU32, n: i32) {
    // SAFETY: the kernel only uses the address as a key for its wait queue; nothing is written.
    unsafe {
        libc::syscall(libc::SYS_futex, state.as_ptr(), libc::FUTEX_WAKE | libc::FUTEX_PRIVATE_FLAG, n);
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::cell::UnsafeCell;

    struct Guarded {
        state: AtomicU32,
        n: UnsafeCell<u64>, // touched only between lock and unlock
    }
    // SAFETY: every access to `n` is between lock(&state) and unlock(&state).
    unsafe impl Sync for Guarded {}

    #[test]
    fn four_threads_lose_no_increment() {
        let g = Guarded { state: AtomicU32::new(UNLOCKED), n: UnsafeCell::new(0) };
        std::thread::scope(|s| {
            let g = &g; // the whole struct, so the closure sees Guarded's Sync and not its fields'
            for _ in 0..4 {
                s.spawn(move || for _ in 0..50_000 {
                    lock(&g.state);
                    // SAFETY: the lock is held, so no other thread is inside.
                    unsafe { *g.n.get() += 1 };
                    unlock(&g.state);
                });
            }
        });
        assert_eq!(g.n.into_inner(), 200_000);
        assert_eq!(g.state.load(Relaxed), UNLOCKED);
    }
}

//! The atomic and cell types that `spsc` and `freelist` are written against:
//! loom's in a build made with `RUSTFLAGS="--cfg loom"`, so the model checker
//! sees every access, and std's otherwise, at no cost.
//!
//! loom's `UnsafeCell` hands out its pointer through `with` and `with_mut`,
//! which take closures, so that it can check each access against every
//! conflicting one. std's has `get` instead, so the std side wraps it in the
//! same two methods.

#[cfg(loom)]
pub use loom::cell;
#[cfg(loom)]
pub use loom::sync::atomic;
#[cfg(loom)]
pub use loom::sync::Arc;

#[cfg(not(loom))]
pub use std::sync::atomic;
#[cfg(not(loom))]
pub use std::sync::Arc;

#[cfg(not(loom))]
pub mod cell {
    /// std's `UnsafeCell` with loom's interface; both methods inline to `get`.
    #[derive(Debug, Default)]
    pub struct UnsafeCell<T>(std::cell::UnsafeCell<T>);

    impl<T> UnsafeCell<T> {
        pub const fn new(v: T) -> Self { UnsafeCell(std::cell::UnsafeCell::new(v)) }

        /// A read: `f` gets a pointer it may read through.
        #[inline(always)]
        pub fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R { f(self.0.get()) }

        /// A write: `f` gets a pointer it may write through.
        #[inline(always)]
        pub fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R { f(self.0.get()) }
    }
}

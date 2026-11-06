// The counting allocator, plus FREES, installed for this test binary alone.
use ch05_structures::counting::Counting;
pub use ch05_structures::counting::{ALLOCS, FREES};

#[global_allocator]
static GLOBAL: Counting = Counting;

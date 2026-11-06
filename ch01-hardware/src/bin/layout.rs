//! layout: section 1.4's size-and-alignment program, its three lines as
//! printed, then the other sizes the chapter states.
//!
//!     layout

use std::mem::{align_of, size_of};
use std::num::NonZeroU64;
use std::sync::atomic::AtomicU64;

use ch01_hardware::{predict, Adjacent, CachePadded, Cli, Counters, OrderC, OrderRust, OrderSorted, Padded, PriceLevel};

fn main() {
    let mut cli = Cli::new("layout", "layout: section 1.4's size-and-alignment program.

usage: layout

Prints size_of and align_of for OrderC, OrderSorted and OrderRust, one line each,
as the chapter's program does, then the other layouts the chapter states.".to_string());
    if let Some(f) = cli.flag() { cli.fail(&format!("unknown flag {f}")); }

    predict("OrderC 40 8, OrderSorted 24 8, OrderRust 24 8 (the three lines below); then PriceLevel \
             32 8, Counters 128, CachePadded<AtomicU64> 128 on x86-64, Option<u64> 16, Option<NonZeroU64> 8.");
    println!("{} {}", size_of::<OrderC>(), align_of::<OrderC>());           // 40 8
    println!("{} {}", size_of::<OrderSorted>(), align_of::<OrderSorted>()); // 24 8
    println!("{} {}", size_of::<OrderRust>(), align_of::<OrderRust>());     // 24 8
    println!();
    let row = |name: &str, size: usize, align: usize| println!("{name:<24} size {size:>4}  align {align:>4}");
    row("PriceLevel", size_of::<PriceLevel>(), align_of::<PriceLevel>());
    row("Padded<AtomicU64>", size_of::<Padded<AtomicU64>>(), align_of::<Padded<AtomicU64>>());
    row("Counters", size_of::<Counters>(), align_of::<Counters>());
    row("CachePadded<AtomicU64>", size_of::<CachePadded<AtomicU64>>(), align_of::<CachePadded<AtomicU64>>());
    row("Adjacent (Lab 1.3)", size_of::<Adjacent>(), align_of::<Adjacent>());
    row("Option<u64>", size_of::<Option<u64>>(), align_of::<Option<u64>>());
    row("Option<NonZeroU64>", size_of::<Option<NonZeroU64>>(), align_of::<Option<NonZeroU64>>());
    println!();
    println!("A million OrderC take {} MB, a million OrderSorted {} MB.",
             size_of::<OrderC>(), size_of::<OrderSorted>());
}

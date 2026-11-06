// tests/first_touch.rs: Lab 2.3's fault counts, small. Faults are counted per thread.
use ch02_os::mem::{self, MapFlags, HUGE, PAGE};
use ch02_os::procfs::faults;
use ch02_os::{full_write, store_per_page};

const SIZE: usize = 16 << 20;

fn faults_of(f: impl FnOnce()) -> u64 {
    let (a, _) = faults();
    f();
    faults().0 - a
}

#[test]
fn first_pass_faults_once_per_page_and_the_second_not_at_all() {
    let mut r = mem::map(SIZE, MapFlags::default()).expect("a plain anonymous mapping");
    let buf = r.bytes_mut();
    let first = faults_of(|| store_per_page(buf));
    let second = faults_of(|| store_per_page(buf));
    let third = faults_of(|| full_write(buf));
    // THP `always` would back this with 2 MB pages and fault 8 times; `madvise` or `never`, 4,096
    assert!(first == (SIZE / PAGE) as u64 || first == (SIZE / HUGE) as u64, "first pass: {first} faults");
    assert_eq!((second, third), (0, 0));
}

#[test]
fn populate_moves_the_faults_into_the_mmap() {
    let mut region = None;
    let in_mmap = faults_of(|| region = Some(mem::map(SIZE, MapFlags { populate: true, ..Default::default() }).expect("map")));
    let mut r = region.expect("mapped");
    let first = faults_of(|| store_per_page(r.bytes_mut()));
    assert!(in_mmap >= (SIZE / HUGE) as u64, "MAP_POPULATE took only {in_mmap} faults");
    assert_eq!(first, 0);
}

#[test]
fn huge_pages_degrade_with_a_message() {
    match mem::map(SIZE, MapFlags { hugetlb: true, ..Default::default() }) {
        Ok(mut r) => assert_eq!(faults_of(|| store_per_page(r.bytes_mut())), (SIZE / HUGE) as u64),
        Err(e) => assert!(e.contains("nr_hugepages") && e.contains("needs 8"), "unclear refusal: {e}"),
    }
    let r = mem::map(SIZE, MapFlags { align_2m: true, ..Default::default() }).expect("map");
    assert_eq!(r.as_ptr() as usize % HUGE, 0, "not 2 MB aligned");
    match mem::advise_huge(&r) {
        Ok(msg) | Err(msg) => assert!(msg.contains("transparent_hugepage"), "{msg}"),
    }
}

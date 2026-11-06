# ch01-hardware

The code for Chapter 1, "The machine underneath: memory and the CPU". The
chapter holds the reasons; this crate holds the code.

```text
Cargo.toml              crossbeam-utils (CachePadded), libc (sched_setaffinity, madvise)
src/lib.rs              the chapter's code blocks, as printed:
                          PriceLevel; OrderC, OrderSorted, OrderRust          (1.4)
                          Padded<T>, Counters                                 (1.5)
                          sum1, sum4                                          (1.6)
                          Side; select_px, count_above_128, apply_fill,
                          best_px: the four branch-free forms, one function each (1.7)
                        what the labs time:
                          stride_sum, flush_touched, STRIDES                  (Lab 1.1)
                          sum_branchy, sum_branchless, sum_vector, keep,
                          counting_sort                                       (Lab 1.2)
                          Adjacent, PaddedPair, hammer, run_pair              (Lab 1.3)
                          Node, List, list_sum, vec_sum, boxes, boxes_sum     (Lab 1.4)
                        and what every binary shares: now() (lfence; rdtsc), calibrate(),
                        pin / pin_as / pin_line, Summary (count, p50, p99, p99.9, max),
                        Rng (SplitMix64), random_bytes, permutation, Buffer (2 MB-aligned,
                        optionally MADV_HUGEPAGE), thp_mode, anon_huge_kb, Cli
src/ring.rs             section 1.5's try_push, verbatim, in the smallest ring that runs it:
                        1,024 slots, head and tail on Padded lines, cached_tail / cached_head
src/bin/stride.rs       Lab 1.1
src/bin/branch.rs       Lab 1.2
src/bin/false_sharing.rs  Lab 1.3
src/bin/chase.rs        Lab 1.4 (stretch)
src/bin/sum.rs          section 1.6's worked example, sum1 against sum4 (not a lab exercise)
src/bin/layout.rs       section 1.4's size_of/align_of program (not a lab exercise)
tests/correctness.rs    the layouts the chapter states; every timed variant against the plain
                        loop; the ring in order across two threads; percentiles; no stack
                        overflow dropping a long list
tests/bins.rs           every binary at a small size: prediction first, percentiles after, --help
runs/                   one run of each binary on the VM this was written on (see the end)
```

```bash
cargo build --release -p ch01-hardware
cargo test --release -p ch01-hardware        # a few seconds once built
cargo clippy --release -p ch01-hardware --all-targets -- -D warnings
```

## The lab

The rules from section 1.10: predict before you measure; build in release;
warm up (every binary makes one untimed pass first); pin; set the governor to
`performance` and turn turbo off if you can, and note it if you cannot; run
each binary three times, and if the p50s disagree by more than about 5%,
something is interfering. Never report a mean.

Every binary prints, in this order: a `Predict first` line with the chapter's
own prediction, the machine (CPU, online CPUs, TSC rate, transparent-huge-page
mode), how it was pinned, and then count, p50, p99 and max for each
measurement. `--help` lists the flags. A pin that cannot be had (the core does
not exist here, or a cpuset withholds it) is a warning, not a stop: the report
says the numbers are from an unpinned thread. Sizes default to the chapter's;
MB means 2^20 bytes.

| Lab | Command | What it does |
|---|---|---|
| 1.1 | `taskset -c 3 ./target/release/stride` | 256 MB of `u64`, every page written; for strides of 1, 2, 4 … 4,096 elements, time per pass and per access. `--size MB`, `--reps N` (11), `--core N\|none` (3), `--huge`, `--no-flush` |
| 1.2 | `perf stat -e branches,branch-misses,cycles,instructions ./target/release/branch --variant unsorted` | 100 MB of random bytes, every byte > 128 summed. Variants `unsorted`, `sorted`, `branchless` (the `cmov` rewrite), `vector`; without `--variant`, all four. `--size MB`, `--reps N` (11), `--seed S` (7), `--core` |
| 1.3 | `./target/release/false_sharing --cores 2,3` | two threads, 100 million `fetch_add(1, Ordering::Relaxed)` each. Variants `adjacent`, `padded` (`CachePadded`), `align64` (the chapter's `Counters`). `--iters N`, `--block N` (1,000,000 increments a sample), `--cores A,B` |
| 1.3 | `perf c2c record -- ./target/release/false_sharing --variant adjacent --cores 2,3`, then `perf c2c report` | the hot line, with offsets 0x0 and 0x8 |
| 1.4 | `./target/release/chase`, then `./target/release/chase --no-shuffle` | 10 million nodes: `vec` (`Vec<u64>`), `list` (one `Box` per node, links shuffled), `boxes` (`Vec<Box<u64>>`, section 1.4's case in between). `--nodes N`, `--reps N` (5), `--seed S`, `--core` |
| 1.4 | `GLIBC_TUNABLES=glibc.malloc.hugetlb=1 ./target/release/chase --huge` | the same with huge pages: `--huge` for the `Vec`, the glibc tunable (2.35 or later) for the malloc'd nodes |
| 1.6 | `./target/release/sum` | `sum1` and `sum4` over 4,096 `f64`, one call a sample. `--len N`, `--iters N` |
| 1.4 | `./target/release/layout` | the chapter's program: `40 8`, `24 8`, `24 8`, then the other sizes it states |

Run `branch` under `perf stat` one variant at a time (`sorted`, `branchless`
too). perf counts the whole run, reps + 1 passes plus the untimed fill and
counting sort, which add almost no branch misses; the binary prints the
expected misses per pass. To read the assembly, as the lab's pitfall asks:
`objdump -d -C -M intel --no-show-raw-insn target/release/branch | grep -A30 'sum_branchy>:'`.
The release build here shows `jbe` per byte in `sum_branchy`, `cmovb` in
`sum_branchless` and SSE2 (`pminub`, `paddq`) in `sum_vector`; build with
`RUSTFLAGS="-C force-frame-pointers=yes -C target-cpu=native"` for AVX2
(an explicit `RUSTFLAGS` replaces `.cargo/config.toml`'s, so repeat the frame pointers).

### Choices the chapter leaves open

- `stride` flushes, untimed, every line a pass will touch before the pass
  (`clflushopt`, or `clflush` where the CPU lacks it). Without that, a large
  stride's few thousand lines (512 KB at 4,096 elements) stay in L2 or L3
  between passes, and the chapter's "every access misses to DRAM" would not
  hold. `--no-flush` shows the difference.
- The branchy loop carries an empty `asm!` statement in its taken arm, and the
  `cmov` loop one per byte. Neither emits an instruction; they stop LLVM from
  turning the branch into a `cmov` or vector code by itself (the pitfall the
  lab names) and keep the rewrite scalar. `vector` is the select left to LLVM.
- `false_sharing` times each thread in blocks of `--block` increments, so the
  percentiles are over 200 samples a variant at the default size; the wall
  time over both threads is the number the chapter predicts. It adds
  `align64`, the chapter's own `Counters`, beside the lab's `CachePadded`.
- `chase` adds `boxes`, for section 1.4's "expect 10-40x slower than `Vec<T>`".
- Default cores: 3 for the single-thread binaries, as in the chapter's
  `taskset -c 3`; when the process was started on one CPU (by `taskset`), that
  CPU is kept. `false_sharing` defaults to cores 2 and 3. `--core none` runs unpinned.

## What needs root or perf

- `perf stat` and `perf c2c`: the perf tool, and for `c2c` usually root or
  `kernel.perf_event_paranoid` at 0 or below. Inside a VM the hardware counters
  are often missing (`<not supported>`).
- Governor and turbo (`cpupower frequency-set -g performance`, turning turbo off): root.
- Huge pages: `--huge` needs no root while
  `/sys/kernel/mm/transparent_hugepage/enabled` says `madvise` or `always`;
  `never` needs root to change. The list's nodes come from malloc: use the glibc
  tunable above, or THP `always` (root). Each run prints `AnonHugePages`, the
  process's memory actually on huge pages.
- Pinning needs no root, only a core the process is allowed.

## Check

The chapter's lab has no "Check:" paragraph; each exercise ends with what to
write down, and that is the check:

- Lab 1.1: where time per pass stops being flat, the knee, and the plateau.
- Lab 1.2: time, branch misses and IPC for all three. If unsorted-branchy is not
  clearly slower, read the assembly before concluding anything.
- Lab 1.3: both times, the ratio, and the cores used.
- Lab 1.4: time per element for both, with and without huge pages.

## runs/

`runs/<bin>.txt` came from a 2-vCPU KVM guest (Intel Xeon, 2.8 GHz TSC), with
no isolated cores, no governor control and no perf, while other builds ran on
the same two vCPUs (each file records the load average). Threads were pinned
to the guest's vCPUs, but a vCPU is not a core: the host can move it, and the
two may be hyperthreads of one host core. Read them as a demonstration that the
binaries run, not as numbers for the chapter. What they showed:

- stride: flat time per pass to stride 16 (26-38 ms for 256 MB), then about
  13 ns per access from 128 B on, with no separate knee at 4 KB; `--huge` put
  all 256 MB on huge pages and did not lower the plateau.
- branch: unsorted-branchy 538 ms per pass against 69 ms sorted and 86 ms
  branchless: 6-8x, inside the chapter's range.
- false_sharing: adjacent 2.6 s against 1.2 s padded, about 2x, below the
  chapter's 5-20x; the padded increment itself took 12 ns, not 5, which fits
  two vCPUs sharing one host core and its L1, the hyperthread pitfall the lab names.
- chase: list 335 ns per node, 269x the `Vec` (220 ns and 213x in an earlier,
  less loaded run); 7 ns and 6x without the shuffle; `Vec<Box<u64>>` 20x; with
  huge pages the list took 205 ns.
- sum: `sum1` 4.8 us, `sum4` 1.2 us for 4,096 values: 3.9x, and the two sums
  differ in the last bits.

# ch02-os: the platform underneath

The code for Chapter 2's lab. The chapter holds the reasons; this crate holds the code.

```text
Cargo.toml
src/lib.rs             now(), now_unfenced(), calibrate(), ToNs, clock_pair_p50(); new_hist(), above(),
                       summary(), quantile(); allowed_cores(), pin_to(), place(), Placement; print_platform();
                       jitter() (Lab 2.1's loop); STRIDES, walk(), walk_logged(), Log, How (Lab 2.2);
                       store_per_page(), store_per_page_timed(), full_write() (Lab 2.3);
                       poll() and TryPop (the spin loop printed in "Not waiting");
                       Wait, ping_pong(), WakeRun (Lab 2.4)
src/counting.rs        the counting allocator printed in "The allocator in between", as printed, with its
                       #[global_allocator]: every binary and test of this crate counts its allocations
src/mem.rs             map() with MAP_POPULATE / MAP_HUGETLB, advise_huge() (MADV_HUGEPAGE), lock_all()
                       (mlockall), hold_c0() (/dev/cpu_dma_latency), set_timer_slack(), cpuidle_states()
src/procfs.rs          /proc/interrupts and /proc/softirqs per core, the thread's ctxt switches and
                       nr_migrations, steal, getrusage fault counts, smaps_rollup and meminfo fields
src/bin/jitter.rs      Lab 2.1, jitter histogram
src/bin/syscalls.rs    Lab 2.2, count the syscalls
src/bin/first_touch.rs Lab 2.3, first touch
src/bin/wake.rs        Lab 2.4 (stretch), wake latency
tests/                 jitter_no_alloc.rs, walk_allocs.rs, wake.rs (each alone in its binary, since the
                       allocation counter sees every thread), first_touch.rs (fault counts), poll.rs
                       (the two printed code blocks, run)
runs/                  one saved run of each binary (see below)
```

Build, test and lint from the workspace root:

```bash
cargo build --release -p ch02-os
cargo test --release -p ch02-os
cargo clippy --release -p ch02-os --all-targets -- -D warnings
```

Every binary takes `--help`, starts with a "Predict first" line holding the chapter's own prediction, records
the kernel, the command line, bare metal or VM, the clock source, the isolated and `nohz_full` cores and THP
mode, says which core the run used (pinned with `--core`, pinned by `taskset`, or unpinned), and ends with the
`/proc/interrupts` and `/proc/softirqs` deltas for that core, the thread's context switches and migrations,
and `steal`. Percentiles are p50, p99, p99.9, p99.99 and max with the sample count, never a mean.

## The lab

| Bin | The chapter's run |
|---|---|
| `jitter` | `./target/release/jitter`, then `taskset -c 3 ./target/release/jitter`, then `taskset -c <isolated core> ./target/release/jitter` |
| `syscalls` | `strace -c -f ./target/release/syscalls`, `perf trace -s ./target/release/syscalls`, `perf stat -e raw_syscalls:sys_enter,page-faults,context-switches,cpu-migrations ./target/release/syscalls`; then each with `--log-pass` and with `--log-access` |
| `first_touch` | `perf stat -e page-faults,dTLB-store-misses ./target/release/first_touch [--populate \| --thp \| --hugetlb \| --mlockall]` |
| `wake` | `./target/release/wake --cores 3,4`, then the same with `--c0`; then `./target/release/wake --sleep --timerslack 1 --cores 3,4` |

**jitter** `[--secs S] [--core N]`. Reads `rdtsc` back to back for 10 s (`--secs`), every gap into a 1 ns..1 s
histogram whose counters were all written before the loop; prints the gaps above 1 µs, 10 µs and 100 µs with
their rate per second, the gaps that came out backwards, and the allocations in the loop (0).

**syscalls** `[--log-pass] [--log-access] [--format] [--buffered] [--quiet] [--size BYTES] [--passes N] [--core N]`.
Chapter 1's stride walk: 256 MB of `u64` (`--size`), every page touched, then 3 passes (`--passes`) at each
stride of 1, 2, 4, … 4,096 elements. The results are printed after the walk, so the plain walk makes no call
from inside the loop. `--log-pass` is one `println!` per pass, `--log-access` one per 1,000 accesses.
`--format` is the chapter's follow-up, `format!` then `write_all` (one allocation per line); `--buffered`
wraps stdout in a `BufWriter`, the "fair experiment of its own". `--quiet` skips the platform header and
the `/proc` snapshots, whose `openat`/`read`/`close` would otherwise sit in strace's table beside the walk's
own calls. The allocation count per stride, the faults of the touch and of the walk, and the log-line count
are printed; the system-call counts come from `strace`/`perf`.

**first_touch** `[--populate] [--thp] [--hugetlb] [--mlockall] [--size BYTES] [--runs N] [--core N]`.
1 GB (`--size`), three runs (`--runs`) of: the `mmap` (timed, for `--populate` and `--mlockall`), one store per
4 KB page, the same pass again, a full sequential write; each with the thread's minor-fault count from
`getrusage`. Then one more run with `now()` around every store, for the per-page distribution of both page
passes. Prints `AnonHugePages` and `Locked` from `/proc/self/smaps_rollup`. `--thp` maps 2 MB-aligned so THP
can back the whole range. A refused `MAP_HUGETLB` or `mlockall` (or a mapping refused under `MCL_FUTURE`) is
printed with the pool size or the lock limit and the fix, and the run continues without it, labelled
"these numbers are NOT --hugetlb".

**wake** `[--spin] [--park] [--sleep] [--c0] [--timerslack NS] [--cores A,B] [--iters N] [--warmup N]`.
Cores 3 and 4 by default (the feed handler's and the order gateway's in "The quiet core"); 1,000,000 round
trips after 10,000 of warm-up. With no variant flag all three run, spin first, and park and sleep are also
shown as a ratio to the spin. Warns if A and B are hyperthread siblings, lists the far core's C-states and
exit latencies, and reports each thread's timer slack and context switches. A core that is not available
is a warning and an unpinned thread, labelled.

## What needs root, perf or strace

- `perf` (none on the VM these were written on): `perf trace`, `perf stat` with `raw_syscalls:sys_enter`
  (root, or `kernel.perf_event_paranoid` at -1), `dTLB-store-misses`. The binaries print the per-thread
  counts they can read themselves: faults from `getrusage` (which, unlike perf's `page-faults`, also count
  the faults `MAP_POPULATE` and `mlockall` take inside the kernel), context switches and migrations from
  `/proc/thread-self`, interrupts from `/proc/interrupts`.
- `strace`: `strace -c -f` counts; never time anything under it.
- Root: an isolated `nohz_full` core for Lab 2.1's third run (boot parameters and a reboot); `--hugetlb`
  needs the pool reserved (`echo 512 > /proc/sys/vm/nr_hugepages` for 1 GB); `--mlockall` beyond
  `ulimit -l` needs `ulimit -l unlimited` or `CAP_IPC_LOCK`; `--c0` needs write access to
  `/dev/cpu_dma_latency`; the performance governor.

## Check

The chapter has no "Check:" paragraph; its lab ends each exercise with what to write down, restated here.
Every run: the kernel, the command line, bare metal or VM, the core, and the `/proc/interrupts` deltas for
that core (the binaries print all of these).

- Lab 2.1: p50, p99.99, max, the count above 1 µs and the `LOC` delta, for each of the three runs. p50 and
  p99 should sit at 8–12 ns and not move between them; the counts and the max are the result.
- Lab 2.2: the syscall counts, the slowdown of each logging variant, and the slowdown `strace` adds to each.
  The allocation count must not move except with `--format`, where it rises by one per line.
- Lab 2.3: the fault count and the three pass times for every variant, and the cost per 4 KB page implied
  by each.
- Lab 2.4: the three round-trip p50s and p99s with and without `--c0`, and the ratio of each to the spin.

## The saved runs

`runs/*.txt` came from an unpinned 2-vCPU Firecracker VM (KVM, kernel 6.18, `HZ` about 250 by the `LOC`
delta, THP `madvise`, no huge-page pool, no cpuidle states, no `nohz_full`), shared with other builds, so
every tail in them is partly the host's and partly the neighbours'. They show the binaries working, not
the chapter's numbers: there is no isolated core for Lab 2.1's third run, `--hugetlb` is refused and
degrades, `--c0` changes nothing because the guest has no C-states to cap, and the mlockall refusal was
shown by dropping `CAP_IPC_LOCK` with `setpriv`, since the VM runs as root.

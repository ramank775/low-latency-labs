# ch03-concurrency: sharing between threads

The code for Chapter 3, *Sharing between threads: how cores agree about memory,
and what agreeing costs*. The chapter holds the reasons; this crate holds the
code: the chapter's code blocks in the library, one binary per lab exercise,
and the loom models that check the orderings against the memory model rather
than against this machine.

```text
Cargo.toml              hdrhistogram, libc; loom only under cfg(loom); the two break-the-ring cfgs
src/lib.rs              now() (lfence + rdtsc; cntvct_el0 on AArch64), calibrate(), new_hist(),
                        report(), report_per(), Padded<T> (128 bytes), SpinBarrier, predict(),
                        OwnCounter (3.9, as printed)
src/atomics.rs          3.3: the data race Rust refuses (a compile_fail doctest of the chapter's
                        block, E0499), next_id_racy() and next_id() for the order-id counter
src/litmus.rs           3.4, 3.5, Lab 3.1: Mp, Sb, mp_write_relaxed/mp_write_release,
                        mp_read_relaxed/mp_read_acquire, sb_release_acquire/sb_seqcst; never
                        inlined, so cargo asm shows each variant on its own
src/quote.rs            3.5: QuoteCell, publish() and wait_for(), as printed
src/futex.rs            3.6: UNLOCKED/LOCKED/CONTENDED, lock() and unlock() as printed, over
                        futex_wait() and futex_wake() (Linux)
src/freelist.rs         3.7: FreeList as printed; TaggedFreeList with the printed tagged pop()
                        and pack(), and a push() that bumps the generation the same way
src/seqlock.rs          3.8: SeqLock<N> with its two fences, as printed
src/spsc.rs             Lab 3.4: Ring<N>, try_push() and try_pop() as in the protocol figure
src/sync.rs             the atomic and cell types: loom's under cfg(loom), std's otherwise
src/topo.rs             pinning (sched_setaffinity), siblings / one L3 / two L3s / two sockets
                        from /sys, per-thread context switches from getrusage
src/bin/message_passing.rs   Lab 3.1
src/bin/counter.rs           Lab 3.2
src/bin/pingpong.rs          Lab 3.3
src/bin/spsc.rs              Lab 3.4's benchmark: 100 million items through Ring<16>
tests/mp_loom.rs        Lab 3.1 under loom: the chapter's flag_publishes_data, both patterns
tests/spsc_loom.rs      Lab 3.4 under loom: N = 2, K = 3, and (ignored) N = 16, K = 18
tests/freelist_loom.rs  3.7's "Seeing it": the ABA found in three threads and three slots, and
                        the tagged list passing the same model
tests/seqlock.rs        3.8's torture test: [k, k, k, k], never torn
tests/common/mod.rs     explore(): runs a loom model and prints how many executions it took
runs/                   one file per binary, loom.txt and asm.txt: the output of the commands below
```

`ch05-structures` holds the data-structures chapter's full ring (capacity,
padding, cached counters, batching). `Ring<N>` here is this chapter's smaller
thing: the ordering protocol and nothing else.

## Build and test

From the workspace root:

```bash
cargo build --release -p ch03-concurrency
cargo test --release -p ch03-concurrency
cargo clippy --release -p ch03-concurrency --all-targets -- -D warnings
```

### loom

The chapter's invocation, run from the workspace root, runs every loom test
(the other crates have none):

```bash
RUSTFLAGS="--cfg loom" cargo test --release
RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency -- --nocapture   # this crate, with the counts
```

`RUSTFLAGS` replaces the workspace's frame-pointer flag, so the build differs
from the ordinary one; `--target-dir target/loom` keeps the two from
rebuilding each other. Each model prints how many executions it explored and
how long it took. `LOOM_MAX_PREEMPTIONS` is honoured; when it is unset the
tests use 3, inside the lab's "2 or 3". `LOOM_LOG=trace` prints the
interleaving of a failing execution, and `LOOM_LOCATION=1` adds source lines.
The tests marked `should_panic` are the broken variants and pass when loom
finds the failure: Relaxed message passing, Release/Acquire store buffering,
the untagged free list's ABA, and loom's own false alarm on `SeqCst` loads and
stores, which it treats as `AcqRel` (the lab's pitfall; the `fence(SeqCst)`
model is the one that checks).

Lab 3.4's larger model and its two broken rings:

```bash
LOOM_MAX_PREEMPTIONS=2 RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency \
    --test spsc_loom -- --ignored --nocapture                     # N = 16, K = 18
RUSTFLAGS="--cfg loom --cfg relaxed_head_store" cargo test --release -p ch03-concurrency --test spsc_loom
RUSTFLAGS="--cfg loom --cfg relaxed_tail_load"  cargo test --release -p ch03-concurrency --test spsc_loom
```

The chapter expects N = 16, K = 18 to take minutes. On the 2-vCPU VM that made
`runs/` it had explored 31 million executions in 30 minutes without finishing
or failing, and was stopped there; it is `#[ignore]`d so that the default run
stays in seconds. The preemption bound limits thread switches, not the
values a load may return: loom lets each load of `head` or `tail` read any of
the last seven stores the model allows, and with 18 items a side those
choices multiply. N = 2, K = 3 already reaches full, empty and wrap-around.

`relaxed_head_store` makes the producer's store of `head` `Relaxed`;
`relaxed_tail_load` makes the producer's load of `tail` `Relaxed`. Both should
fail, with loom's "Causality violation: Concurrent read and write accesses":
the first on the consumer's read of a slot the producer wrote, the second on
the producer's write to a slot the consumer is still reading, which needs
K > N to reach.

## The labs

Every binary starts by printing the chapter's prediction ("Predict first"),
the architecture (`uname -m`), the CPUs it may use, and what the chosen cores
share; then p50, p99, p99.9 and max, never a mean, with the sample count.
`--help` lists every flag. Defaults follow the chapter's worked examples:
cores 2 and 3 (and 4 and 5 for the counter). A core this process cannot have
is reported and the thread runs unpinned; an unpinned number is not the lab's
number, so pass cores from the list printed on the `machine:` line.

| Lab | Command |
|---|---|
| 3.1 message passing | `./target/release/message_passing --relaxed` and `--release-acquire` (neither: both) |
| 3.1 store buffering | `./target/release/message_passing --sb` (Release/Acquire, then SeqCst; `--sb --seqcst` for one) |
| 3.1 under loom | `RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency --test mp_loom -- --nocapture` |
| 3.2 contended counter | `./target/release/counter --mutex`, `--atomic`, `--per-thread`, `--per-thread-rmw`, `--unpadded` (none: all five and a table) |
| 3.2 futex calls and context switches | `perf stat -e context-switches,syscalls:sys_enter_futex ./target/release/counter --mutex` (the binary prints getrusage's context switches without perf; `strace -f -c -e trace=futex` also counts, but slows the threads it stops) |
| 3.3 ping-pong | `./target/release/pingpong --cores A,B`, with and without `--no-pause` |
| 3.3 every placement | `./target/release/pingpong --placements` (siblings, one L3, two L3s, two sockets, whichever exist) |
| 3.3 matrix | `./target/release/pingpong --matrix` |
| 3.4 loom | `RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency --test spsc_loom -- --nocapture` |
| 3.4 benchmark | `./target/release/spsc --items 100000000 --cores A,B` |

Sizes default to the chapter's: 100 million message-passing iterations, 10
million store-buffering rounds, 4 threads x 10 million increments, a million
round trips, 100 million ring items. `--iters`, `--rounds` and `--items` make
them smaller.

The assembly behind each Lab 3.1 variant (install `cargo-show-asm` first):

```bash
cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::mp_write_relaxed
cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::mp_write_release
cargo asm -p ch03-concurrency --lib ch03_concurrency::litmus::sb_seqcst      # the xchg
cargo asm -p ch03-concurrency --lib ch03_concurrency::atomics::next_id       # lock xadd
```

## What needs root, perf or other hardware

- `perf` (often needs `perf_event_paranoid` at 1 or lower, or root): `perf stat -e
  machine_clears.memory_ordering` (3.4), `perf stat -e mem_inst_retired.lock_loads` (3.3),
  `perf stat -e context-switches`, `perf trace -s -e futex`, `perf lock contention -b` (3.6),
  `perf c2c` (3.7, 3.8, 3.9). None of the binaries needs perf; the counter prints each
  thread's context switches itself.
- `strace` needs ptrace permission (`kernel.yama.ptrace_scope`), not root, for a child it starts.
- The lab's performance governor, and isolating cores (`isolcpus`, `nohz_full`), need root;
  without them the max carries timer ticks, and a busy box carries preemptions.
- Lab 3.1 on AArch64: the crate builds there (the clock is `cntvct_el0`); an x86 box cannot
  show three of the four reorderings. Miri (`cargo +nightly miri test`) complements loom.
- Two sockets, two L3s or hyperthread siblings exist only on boxes that have them;
  `pingpong --placements` says which it found.

## Check

The chapter has no paragraph headed "Check:". Its lab asks each run to write
down the numbers below, and section 3.9's "How it is checked" says what must
hold; this restates both.

- Lab 3.1: stale reads per 100 million for each variant and architecture (0 for
  `--release-acquire` everywhere, and 0 for `--relaxed` on x86, which is not
  evidence of correctness); both-zero rounds per million for each store-buffering
  variant (above zero for Release/Acquire on x86, zero for SeqCst); ns per
  iteration and per round; loom's execution counts.
- Lab 3.2: the wall time, ns per increment and ratio to `--per-thread` for each
  variant, and the `futex` count for `--mutex`.
- Lab 3.3: p50, p99 and max for each placement, and the ratio of the two-socket p50
  to the one-L3 p50.
- Lab 3.4: the execution counts and times for both loom runs, which broken version
  failed and with what message, and the ns per item.
- Section 3.9: every structure that uses an ordering has a loom test that drives it
  through full, empty and wrap-around with two or three threads; the unit tests run
  under Miri too; Lab 3.1 runs on AArch64 as well as x86-64; `perf c2c` shows HITMs
  only on lines meant to be shared; and the hot threads make no `futex` calls after
  warm-up, with flat `voluntary_ctxt_switches`.

## The runs in `runs/`

Made on an unpinned 2-vCPU KVM guest (Intel Xeon @ 2.80 GHz; one socket, one
L3, no hyperthread siblings visible), whose vCPUs the host schedules as it
likes, while other jobs ran on the same guest. The threads were pinned to the
two vCPUs with `--cores 0,1` (the counter's 4-thread run used its defaults,
which this guest does not have, and so ran unpinned), at the small sizes shown
on each file's `$` lines. The tails and some medians carry the hypervisor and
the neighbours as well as the protocol: read them as what the commands print,
not as the chapter's numbers.

- `message_passing.txt`, `counter.txt` (with the `strace` futex count),
  `pingpong.txt`, `spsc.txt`: one file per binary
- `loom.txt`: every loom model's execution count and time, and the two broken rings' failures
- `asm.txt`: the Lab 3.1 functions disassembled (objdump, as `cargo asm` was not installed)

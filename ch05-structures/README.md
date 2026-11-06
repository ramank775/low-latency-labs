# ch05-structures: Chapter 5, Shaping the data

The data-structures chapter's code and its lab. The chapter has the reasons;
this crate has the code.

```text
ch05-structures/
  Cargo.toml
  src/
    lib.rs          Update, Side, the bitmap helpers (next_set_bit_above/below), Pool,
                    try_push, Symbol, InstrumentId, Position, Interner, Positions, qty_of,
                    FxHasher (hand-written), Pending, bitscan_above / flagscan_above
                    (exercise 5), Strategy (5.8's two exposure loops)
    ring.rs         Ring, ring(), Producer, Consumer: the SPSC ring of 5.6
    counting.rs     the counting global allocator: ALLOCS, FREES, installed()
    arena.rs        5.3's arena (bumpalo) and its Intent
    first_version.rs  5.1's first, allocating Update
    lab.rs          the lab's kit: rdtsc clock and calibration, the clock pair, fixed-memory
                    histograms (p50 / p99 / p99.9 / max, never a mean), pinning, /proc counts
    bench.rs        the two-thread queue harness ring, pad and alloc share; the
                    one-line ring of exercise 2; Mutex<VecDeque<u64>>
    bin/
      ring.rs       exercise 1
      pad.rs        exercise 2
      lookup.rs     exercise 3
      alloc.rs      exercise 4
      bitmap.rs     exercise 5
  tests/
    no_alloc.rs               the ring and the pool allocate nothing after warm-up
    cancel_replace.rs         5.3's pooled cancel-replace allocates nothing
    first_version_allocates.rs  5.1's first Update: two allocations and two frees a message
  examples/
    uninit.rs       5.8's MaybeUninit slots
    exposure.rs     5.8's black_box honesty and the indexed and iterator exposure loops
  runs/             each binary's output at a small size (see below)
```

## Build and test

From the workspace root:

```bash
cargo build --release -p ch05-structures
cargo test --release -p ch05-structures
cargo clippy --release -p ch05-structures --all-targets -- -D warnings
```

The binaries land in `target/release/`; the chapter writes them as `./ring` and
so on. Each starts by printing the chapter's prediction ("Predict first"):
write yours down before reading the numbers below it. Every time is reported
as p50, p99, p99.9 and max with its sample count, never as an average, with
the cost of two `rdtsc` reads back to back (printed at the top) subtracted.

## The lab

The two-thread runs default to cores 2 and 3 (`--cores A,B`; check in
`lscpu -e` that they share an L3), the one-thread runs to core 3 (`--core N`).
A core the machine does not have, or a cpuset withholds, is reported and the
run goes on unpinned, saying so on every run it affects. Pin the whole process
too, as the chapter does, with `taskset`.

```bash
# 1. The ring against mpsc and Mutex<VecDeque<u64>>: 100 million items flat out for
#    throughput, then paced one a microsecond for latency (100 s a queue at the default),
#    then flat out with stamps: against a full ring, latency is only queueing.
taskset -c 2,3 ./target/release/ring                 # or: ring spsc|mpsc|mutex --items N --paced-items N

# 2. head and tail on one cache line, beside the padded ring.
taskset -c 2,3 ./target/release/pad                  # or: pad padded|shared
perf c2c record -- ./target/release/pad shared --items 10000000 && perf c2c report

# 3. Map against array: the chapter's three runs, then again with 100,000 ids.
perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./target/release/lookup map 10000000
perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./target/release/lookup fx 10000000
perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./target/release/lookup array 10000000
perf stat -e cache-misses,L1-dcache-load-misses,instructions -- taskset -c 3 ./target/release/lookup none 10000000   # the loop alone: subtract
./target/release/lookup map 10000000 --ids 100000
./target/release/lookup                              # all four at 64 ids, then at 100,000

# 4. Zero allocation: the ring under the counting allocator, then channel(), then sync_channel(4096).
taskset -c 2,3 ./target/release/alloc                # or: alloc ring|channel|sync; exits 1 if the ring allocated

# 5. Stretch: trailing_zeros against a scan over [bool; 64], distances 1 to 63 fixed, then random.
taskset -c 3 ./target/release/bitmap
cargo asm -p ch05-structures --lib bitscan_above     # tzcnt (rep bsf without target-cpu=native), no loop
cargo asm -p ch05-structures --lib flagscan_above    # a cmp and a jne per flag
perf stat -e branches,branch-misses -- taskset -c 3 ./target/release/bitmap scan random
perf stat -e branches,branch-misses -- taskset -c 3 ./target/release/bitmap bitscan random
```

How each is measured. `ring` and `pad`: flat out, the consumer checks every
item's number and reads the clock once per block of 4,096 items, so throughput
is a per-item time per block; paced, the producer spins to each tick of its
period (open loop), stamps `rdtsc` into the `u64` and pushes it, the consumer
subtracts it on pop. A push that finds no room is retried with a fresh stamp.
The unbounded channel's producer is held to 65,536 items ahead of the consumer,
checked once a block, so it cannot fill the machine's memory; the bounded
queues never reach that. `lookup`: each id is computed from the previous
lookup's result, so lookups cannot overlap, and timed in blocks of 1,000.
`alloc`: the consumer reads `ALLOCS` and `FREES` when the warm-up ends (both
threads running, the histogram already made) and at its last item; the
producer waits for that read before it exits, since a thread's exit frees.
`bitmap`: both searches are `#[inline(never)]` library functions, timed in
blocks of 1,000 calls; a call's own cost is in both.

## What needs root, perf, or another machine

- `perf stat`, `perf record`, `perf c2c`: need `perf`, and `kernel.perf_event_paranoid`
  at 2 or below for your own process (`perf c2c` needs 0 or root). The binaries
  print the same commands in `--help`.
- Pinning to cores 2 and 3 needs a machine with them; `isolcpus`/`nohz_full`
  (boot parameters, root) to keep other work off them.
- `cargo asm` needs `cargo install cargo-show-asm`.
- `cargo +nightly miri test` for the commented line in `examples/uninit.rs`.

## The runs here

`runs/<bin>.txt` came from an unpinned 2-vCPU KVM virtual machine (Xeon at
2.8 GHz, no cores 2 or 3, other builds running beside it), at small sizes:

```bash
ring --items 4000000 --paced-items 200000
pad --items 4000000 --paced-items 200000
lookup 2000000
alloc --items 4000000
bitmap --calls 200000
```

with the default cores, which this machine does not have, so each run also
shows the refusal being reported and the run going on unpinned.

Two spinning threads on two shared vCPUs time-share with everything else, so
every two-thread percentile above the p50 there is the scheduler's, and the
ring's flat-out numbers are not its numbers. They show the binaries working,
not the chapter's ratios.

## Check

Everyone can draw the ring buffer with its two indices and explain which
thread owns which cache line. A good answer says that the producer owns `head`
and the slots it is writing, and the consumer owns `tail` and the slots it is
reading; that each counter sits on a line of its own; that each slot line
crosses once, from producer to consumer, at 50–100 ns, which is the price of
the data; and that the cached copies mean a push reads no line the consumer
writes.

# ch07-capstone: Chapter 7, The number

The capstone: the week's pieces in one loop, measured end to end. The
networking chapter's 48-byte message is decoded, the data-structures chapter's
ring carries it, the book chapter's `ArrayBook` and `Quoter` decide, and every
message is stamped from the moment it is due to the moment its decision is
made. The chapter holds the reasons; this crate holds the code.

```text
Cargo.toml          the `stages` feature
src/lib.rs          now(), now_cpu(), now_unfenced(), calibrate(), stage(); new_hist(), record(),
                    merge(), report(), Worst, Hists; Stamped, StampedOrder, decode(); the replay
                    reader; Run, Batch, Report, the decisions Log; lock_memory()
src/pipeline.rs     the three threads: feed (core 2), strategy (core 3, the loop printed in
                    "The loop, line by line"), gateway (core 6); and how a run is assembled
src/queue.rs        the --queue swaps behind two small traits, QueueTx and QueueRx
src/synth.rs        the synthetic feed gen writes
src/stats.rs        the kernel's per-thread counters, read from /proc without allocating
src/bin/gen.rs      writes the replay file
src/bin/run.rs      assembles, instruments and reports: exercises 1 to 5
tests/              no_alloc.rs (the assembled loop allocates nothing after warm-up),
                    mpsc_allocates.rs (the counter sees the hot threads), decisions.rs
                    (every swap decides the same wherever two runs both decided)
runs/               the runs of the lab below, and determinism.txt, the joins of exercise 4
```

It depends on `ch04-network` (the wire message), `ch05-structures` (the ring,
`Update`, the counting allocator) and `ch06-book` (the books, `FeedHandler`,
`Quoter`).

## Build and test

From the workspace root:

```bash
cargo build --release -p ch07-capstone                                              # headline: t0 and t3 only
cargo build --release -p ch07-capstone --features stages --target-dir target/stages # breakdown: all five points
cargo test --release -p ch07-capstone
cargo clippy --release -p ch07-capstone --all-targets -- -D warnings
```

Only `--release` numbers mean anything. The two builds go to different
directories so neither binary overwrites the other. `gen --help` and
`run --help` print the usage.

## The replay file

```bash
./target/release/gen                       # 2,000,000 records, 112 MB, into replay.bin
./target/release/gen --records 200000 --rate 100000 --seed 1 --out target/replay.bin
```

Each record is 56 bytes: `t_arrival_ns` (u64, little-endian, nanoseconds since
the first record), then the networking chapter's 48-byte message. The feed is
one instrument, id 42, sequence numbers from 1 with no gaps, quantity 0
removing a level; about 95% of updates land on a deep level, 4% change the best
level's quantity and 1% move the best price, those in bursts of up to 30
messages a few nanoseconds apart. Arrivals are exponential gaps at `--rate` on
average. The same seed writes the same bytes. Converting a capture from the
networking chapter's lab is not implemented.

## run

| Flag | Default | Meaning |
|---|---|---|
| `--paced` / `--flat` | paced | paced: each message released at its recorded arrival, t0 the scheduled tick (open loop); flat: as fast as the queue takes them, t0 read before decode |
| `--book array\|tree\|vec` | array | `ArrayBook` (65,536 levels a side), `TreeBook` (a `BTreeMap` a side), `VecBook` (sorted, preallocated) |
| `--queue spsc\|mutex\|mpsc` | spsc | the ring, `Mutex<VecDeque<Stamped>>`, or `std::sync::mpsc::channel` |
| `--stall-every MS` | 0 (off) | the strategy spins 10 ms every MS milliseconds, the first MS into the run, at the top of its loop |
| `--records N` | the whole file | replay at most N records |
| `--warmup N` | 100000 | the first N messages run through every line and are left out of every histogram |
| `--file PATH` | replay.bin | the replay file |
| `--decisions PATH` | none | one line per batch: `seq bid_px bid_qty ask_px ask_qty want_bid want_ask` |
| `--orders PATH` | none | one line per order the strategy sent: `seq side price qty`, seq closing its batch |
| `--feed-core N` | 2 | `none` for unpinned |
| `--strategy-core N` | 3 | |
| `--gateway-core N` | 6 | |

`run` prints the run's configuration and the machine (CPU, kernel, TSC rate,
clocksource, governor, isolated and `nohz_full` cores, a hash of the binary);
the p50 of back-to-back `now()` pairs, to subtract from every stage; updates per
second and batches; the five histograms as count, p50, p99, p99.9, max and the
sequence number of the max (in ns, after warm-up; `decode`, `hop` and `think`
only in the stages build; `out_hop` has no sequence number, since an order does
not carry one); the worst 100 `total` samples as `ns@seq`; and the counters that
must read zero: sequence gaps, orders dropped on a full output ring,
intervals that came out backwards (a later stamp below an earlier one, which
only two cores' counters disagreeing can produce; recorded as 0 ns, not as a
false 1 s), allocations after warm-up, and for each thread page faults,
context switches and migrations after warm-up.

A pin that cannot be had stops the run, whether the core does not exist here,
a cpuset withholds it, or the kernel refuses it: numbers for an unpinned thread
are never labelled as pinned. On a machine without cores 2, 3 and 6, name cores
it has, or `none` to run a thread unpinned on purpose; the report says which.

## Before trusting any number above the p50

- Cores: the feed, strategy and gateway each need a core of their own, feed
  and strategy sharing an L3 (`lscpu -e`). Three threads spin at 100% for the
  whole run; with fewer than four cores they time-share and every percentile
  above the median is the scheduler's.
- Isolation (boot parameters, a reboot, root): `isolcpus=2,3,6 nohz_full=2,3,6
  irqaffinity=0,1` or your machine's equivalent; check
  `/sys/devices/system/cpu/isolated`.
- Clock rate (root): `cpupower frequency-set -g performance`, and turbo off:
  `echo 1 > /sys/devices/system/cpu/intel_pstate/no_turbo` (Intel) or
  `cpupower set --turbo-boost 0` (AMD).
- Memory: `run` calls `mlockall(MCL_CURRENT | MCL_FUTURE)` first and `mlock`s
  the replay file. That needs root, `CAP_IPC_LOCK`, or `ulimit -l unlimited`;
  without it `run` warns and pages may fault during the run.
- The clock: `grep -o 'constant_tsc\|nonstop_tsc' /proc/cpuinfo | sort -u` must
  print both, and `current_clocksource` should say `tsc`.

Needs root: isolation, the governor and turbo settings, `mlockall` beyond the
locked-memory limit, `dmesg`, and `perf stat -a` / `perf record -C` unless
`kernel.perf_event_paranoid` is 0 or below. Needs `perf`: exercise 3 and the
whole-core counts; `run` reads per-thread counters from /proc itself.

## The lab

```bash
# 1. Assemble: flat, the stages build. Updates per second, think, the zero counters,
#    and the batch count (decisions-file lines) beside the message count.
./target/stages/release/run --flat --decisions flat.decisions

# 2. Instrument: both builds, paced. Then cut the slowest message and the 100,000
#    before it, so it lands just after the warm-up, and replay that alone.
./target/release/run --paced
./target/stages/release/run --paced
k=$(( SEQ_OF_MAX - 1 ))                      # record k starts at byte 56 * k; the file starts at seq 1
dd if=replay.bin of=slow.bin bs=56 skip=$((k - 100000)) count=100001
./target/stages/release/run --paced --file slow.bin    # idles until the first arrival comes round

# 3. Profile (needs perf; frame pointers and debug = 1 are already set).
sudo perf record -F 4000 -g -C 3 -o strategy.data -- ./target/release/run --flat
sudo perf report -i strategy.data --no-children
sudo perf script -i strategy.data | inferno-collapse-perf | inferno-flamegraph > flame.svg
sudo perf annotate -i strategy.data --stdio '<ch06_book::ArrayBook as ch06_book::Book>::apply'
sudo perf stat -a -C 3 -D 2000 -e cycles,instructions,cache-misses,branch-misses,context-switches,cpu-migrations,page-faults \
    -- ./target/release/run --paced

# 4. Vary: one swap at a time, paced, and prove the decisions did not change.
./target/release/run --paced --decisions base.decisions
./target/release/run --paced --book tree  --decisions tree.decisions
./target/release/run --paced --queue mutex --decisions mutex.decisions
./target/release/run --paced --queue mpsc  --decisions mpsc.decisions
./target/release/run --paced --book vec   --decisions vec.decisions
export LC_ALL=C
join -j1 <(sort -k1,1 base.decisions) <(sort -k1,1 tree.decisions) |
  awk '{n++; if ($2!=$8||$3!=$9||$4!=$10||$5!=$11||$6!=$12||$7!=$13) bad++}
       END {print n, "joined,", bad+0, "differ"}'      # must say 0 differ

# 5. Break it honestly: the same stall, open loop and closed.
./target/stages/release/run --paced --stall-every 1000
./target/stages/release/run --flat  --stall-every 1000   # over before the first stall is due
./target/stages/release/run --flat  --stall-every 50     # with the default 2,000,000-record file
```

Exercise 6 is reading `ch07-capstone/src/pipeline.rs`, the `strategy` function.

Why the join, and not `cmp`: where a batch ends depends on timing, so two
runs, even of the same binary, close their batches at different updates, and
the quoter sends a side only when the quote it wants has changed since the last
batch. The orders files therefore differ wherever the batch boundaries do. The
quote wanted after a given update is a function of the book alone, so wherever
two runs both decided at the same sequence number, their decisions lines must
match exactly.

## runs/

One file per run of the lab above, from a 2-vCPU KVM guest (Xeon at 2.1 GHz,
no isolated cores, other builds running) on a 200,000-record file
(`gen --records 200000 --seed 7 --out target/replay.bin`), with
`--feed-core 0 --strategy-core 1 --gateway-core none`: three spinning threads
on two vCPUs. Each `.txt` beside its `.decisions`; `determinism.txt` holds the
joins of exercise 4, every one `0 differ`. They show the shape of each result,
not its tail: every percentile above the p50 there is the scheduler's.

## Where the code differs from the printed text

- The book chapter's `Update` had no `instrument`; this workspace has one `Update`,
  the data-structures chapter's 32-byte one, used by the book crate too.

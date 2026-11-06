# ch06-book: Chapter 6, The book

The book chapter's code and its lab. The chapter has the reasons; this crate
has the code.

```text
ch06-book/
  Cargo.toml        the four binaries, declared by name (see below)
  src/
    lib.rs          Ticks, Lots, Top, the Book trait; ArrayBook (the dense array: window,
                    cursors, bitmaps), TreeBook (BTreeMap a side), VecBook (sorted Vec);
                    check_seq, crossed, joins, BookState, FeedHandler (on_update,
                    on_snapshot), Order, Quoter; the lab's additions: ArrayBook::rebase_copy,
                    ArrayBook::rebuild, qty_at, RingBook (the window as a ring),
                    TreeBook::snapshot, timed_replay / untimed_replay; the property tests
    gen.rs          the generator of updates (95% deep, 4% the best level's quantity, 1%
                    moving the best price; seeded), the property test's stream, and the
                    56-byte replay record, read and written
    bin/
      build.rs      exercise 1   -> target/release/book-build
      replay.rs     exercise 2   -> target/release/book-replay
      break.rs      exercise 3   -> target/release/break
      rebase.rs     exercise 4   -> target/release/rebase
  runs/             each binary's output at a small size (see below)
```

The `Update` is the data-structures chapter's 32-byte one (it carries
`instrument`; this chapter's listing does not), so that one type crosses from
the decoder to the book, as the capstone needs.

## Two binary names: book-build and book-replay

Cargo refuses a binary called `build` (its own `target/release/build`
directory has that name), and the networking chapter's crate already has a
`replay`, which would land on the same `target/release/replay`. So exercise 1
is `book-build` and exercise 2 is `book-replay`, as section 6.9 names them; the
sources keep the file names its layout shows, and `break` and `rebase` keep
theirs. Where the chapter's earlier sections write `./replay array
updates-10m.bin`, run `./target/release/book-replay array updates-10m.bin`.

## Build and test

```bash
cargo build --release -p ch06-book
cargo test --release -p ch06-book          # the property tests: the three books, and the ring
                                           # and both re-bases against an exact windowed model
cargo clippy --release -p ch06-book --all-targets -- -D warnings
```

Each binary starts by printing the chapter's prediction ("Predict first"):
write yours down first. Times are p50, p99, p99.9 and max with the sample
count, never an average, each `apply` between two fenced `rdtsc` reads with
the cost of two reads back to back subtracted. The one-thread runs pin to
core 3 (`--core N`, or `none`); a core the machine lacks is reported and the
run goes on unpinned, saying so.

## The lab

```bash
# 1. Build a book: the same random updates to all three, tops compared after every
#    update; zeros one in three, so the best empties and is found again across words.
./target/release/book-build                          # --records N --seed S --zero-one-in K --band B; exits 1 on a disagreement

# 2. Replay 10 million updates through each: updates per second, then apply's p50 / p99 /
#    max, also split by what the update did to the top (unchanged, best quantity, best moved),
#    with the kernel's faults and context switches during the timed pass, to name a max.
./target/release/book-replay                         # all three, generated, seed 7
./target/release/book-replay --write updates-10m.bin # save the stream as 56-byte records
perf stat -e cycles,instructions,branch-misses,cache-misses,L1-dcache-load-misses \
    -- taskset -c 3 ./target/release/book-replay array updates-10m.bin
perf record -g -- taskset -c 3 ./target/release/book-replay tree updates-10m.bin && perf report
RUSTFLAGS="-C target-cpu=native" cargo asm -p ch06-book --lib "<ch06_book::ArrayBook as ch06_book::Book>::apply"

# 3. Break it: drop the delete of the best bid, watch the copy go wrong and cross; then the
#    feed handler's sequence check, Stale, a snapshot simulated at 70 ms, the replay, Live.
./target/release/break                               # --rate 20000 --snapshot-ms 70 --resyncs 10 --book tree|vec

# 4. Stretch: the window as a ring, against the copy re-base and a rebuild, for a step of a
#    few levels and a jump of the whole window; then a price that walks out of the window.
./target/release/rebase                              # --step 4 --reps 1000 --drift-records 1000000
```

The replay file is the capstone's format: 56-byte records, an arrival time in
nanoseconds since the first record, then the networking chapter's 48-byte
message, little-endian, kind 7. The capstone's `gen` writes the same shape
with bursts and arrival times (`gen --records 10000000 --out updates-10m.bin`),
and `book-replay` reads either. The generator here is its own, since this
crate cannot depend on the capstone, which depends on it.

Exercise 3 runs on the stream's own clock, one update every 1/`--rate`
seconds: the dropped update never arrives, the handler's state decides when a
snapshot is requested, the venue (a `TreeBook` that saw every update) takes it
half-way through the round trip, and it is delivered `--snapshot-ms` after the
request. The handler's book is a `TreeBook` or a `VecBook`: the chapter's
`on_snapshot` starts from `B::default()`, which `ArrayBook`, needing a base,
does not have. Part 1 uses `ArrayBook` with no check at all.

Exercise 4's ring keeps its cursors as prices, since slot order wraps; a
re-base of d ticks clears d slots a side and their bits. The copy re-base
moves both sides' levels with `copy_within` and shifts the bitmaps. The
rebuild clears the whole window and applies the reference book's levels: no
holes, and in production a snapshot's 70-200 ms round trip, which the lab
leaves out.

## What needs root, perf, or another machine

- `perf stat` / `perf record`: need `perf`, and `kernel.perf_event_paranoid` at 2
  or below for your own process. The binaries print the commands in `--help`.
- Pinning to core 3 needs a machine with it; isolation needs boot parameters (root).
- `cargo asm` needs `cargo install cargo-show-asm`.

## The runs here

`runs/<bin>.txt` came from an unpinned 2-vCPU KVM virtual machine (Xeon at
2.8 GHz, no core 3, other builds running beside it), at small sizes:

```bash
book-build --records 1000000
book-replay --records 2000000
break
rebase --reps 500 --drift-records 1000000
```

A max far above its p99 there is the machine: the replay prints the context
switches and page faults of its timed pass beside it.

## Check

Everyone can explain why the dense array wins and what it costs in memory. A
good answer says that the array turns a price into an address by arithmetic,
so an apply is an index, a load, a compare and a store at 2–5 ns with the line
usually in L1, where the tree chases pointers and allocates at 100–500 ns, and
that the price is 512 KB a side (1 MB for both) for a window chosen per
instrument and re-based when the price leaves it, against 54 MB a side for an
array over the whole range.

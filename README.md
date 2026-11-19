# Low-Latency Systems Fundamentals: the lab code

The companion repository for the course book *Low-Latency Systems
Fundamentals*. Each chapter ends with a "Lab and reading" section; its code is
here, one crate per chapter, named as the chapter names it, so the chapter and
the code can be read side by side. The repository holds the code and the book
holds the reasons.

## Prerequisites

- Rust 1.92 or later (`rustup` stable is fine).
- Linux on x86_64: the clocks are `rdtsc` behind `lfence`, and the pinning,
  `/proc` counters and socket options are Linux's. (`ch03-concurrency` also
  builds on AArch64, for Lab 3.1.)
- `perf`, for the steps that count hardware events; `strace` for the
  system-call counts. Every binary runs without them and prints the command
  to run under them in `--help`.
- Optional root (or the matching capability) for huge-page pools, core
  isolation (`isolcpus`, `nohz_full`: boot parameters and a reboot), the
  performance governor and turbo, `mlockall` beyond `ulimit -l`, and
  `/dev/cpu_dma_latency`. Without it each binary says what it could not do and
  carries on, labelled; the capstone's `run` stops on a pin it cannot have.

## Build

From this directory:

```bash
cargo build --release --workspace
cargo build --release -p ch07-capstone --features stages --target-dir target/stages   # the capstone's stage breakdown
cargo test --workspace --release
cargo clippy --release --workspace --all-targets -- -D warnings
RUSTFLAGS="--cfg loom" cargo test --release -p ch03-concurrency --target-dir target/loom  # the loom models
```

Only `--release` numbers mean anything. The binaries land in
`target/release/`, the capstone's stages build in `target/stages/release/`; a
separate `--target-dir` for loom and for stages keeps each build from
overwriting the other's binaries.

```text
Cargo.toml            the workspace; release profile: debug = 1, lto = "thin", codegen-units = 1
.cargo/config.toml    -C force-frame-pointers=yes for every crate, so perf can walk stacks; on x86-64
                      also -x86-branches-within-32B-boundaries, so loop speed does not hang on layout
rustfmt.toml          formatting off: the code keeps the chapters' printed layout, line for line
```

## The crates

| Crate | Chapter | Binaries |
|---|---|---|
| `ch01-hardware/` | 1, Memory and the CPU | `stride`, `branch`, `false_sharing`, `chase`; `sum` and `layout` (sections 1.6 and 1.4) |
| `ch02-os/` | 2, The platform underneath | `jitter`, `syscalls`, `first_touch`, `wake` |
| `ch03-concurrency/` | 3, Sharing between threads | `message_passing`, `counter`, `pingpong`, `spsc`; the loom models in `tests/` |
| `ch04-network/` | 4, Talking between machines | `ladder`, `nagle`, `pollwait`, `parse`, `replay` |
| `ch05-structures/` | 5, Shaping the data | `ring`, `pad`, `lookup`, `alloc`, `bitmap` |
| `ch06-book/` | 6, The book | `book-build`, `book-replay`, `break`, `rebase` |
| `ch07-capstone/` | 7, The number | `gen`, `run` |

Chapter 0, *The machine from the top*, has no lab code. The book chapter's
first two binaries are `book-build` and `book-replay`: Cargo reserves `build`
as a binary name, and `replay` would collide with the networking chapter's.

`ch06-book` uses `ch05-structures`; `ch07-capstone` uses `ch04-network`,
`ch05-structures` and `ch06-book`. Every other crate stands alone, with its own
clock, pinning and histogram helpers, so that each reads as its chapter
describes it.

## How to measure

The rules every lab follows, and every binary is built to make easy:

- Predict first. Each binary (but the capstone's `run`, which prints its
  configuration instead) starts with a `Predict first` line holding the
  chapter's own prediction. Write yours down before reading on.
- Report p50, p99 and max, with p99.9 where the count allows, never an
  average; a mean hides exactly the tail these chapters are about.
- Pin. `--core N` or `--cores A,B`, or `taskset -c`; the defaults are the cores
  the chapter names. A pin that cannot be had is printed, and the numbers are
  labelled unpinned.
- Report the count and the machine: every binary prints its sample count, the
  machine (CPU or architecture, clock rate, the cores it could use) and what
  it pinned, and subtracts the cost of reading the clock twice back to back
  where the chapter says to.

## The runs/ directories

Each crate's `runs/` holds the output of its binaries, made once while the
crates were written. Every one of those files came from a 2-vCPU KVM guest
with no isolated cores and other builds running beside it; where a run was
pinned, it was to one of the guest's two vCPUs, which the host moves as it
likes. They show the shape of each result, not its tail: read the p50s as
this machine's and everything above them as its neighbours'.

## The exercises

Each crate's own `README.md` has its layout, every lab command as the chapter
writes it, the flags, what needs root, `perf` or a second machine, and the
chapter's check: start with the crate for the chapter you are reading.

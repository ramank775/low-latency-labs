# ch04-network — Chapter 4, Talking between machines

The code for every experiment in Chapter 4 is here; the repository holds the code and the chapter holds the reasons.
The library also holds the chapter's 48-byte depth update (`parse`, `price`, `encode`), which `ch07-capstone` uses.

## Layout

```text
ch04-network/
  src/lib.rs        the 48-byte wire message: Update, parse, price, encode (4.7); Fields and read_fields,
                    the function whose x86-64 the chapter prints
  src/calls.rs      4.1  the two sides of a TCP conversation: the_side_that_waits, the_side_that_calls
  src/options.rs    4.4  TCP_NODELAY, TCP_QUICKACK, write_vectored, TCP_CORK, MSG_MORE: the_options
  src/multicast.rs  4.5  join_feed_a, one_datagram, and the A/B Arbiter with its on_packet
  src/waiting.rs    4.6  epoll_feed (mio), spin_feed (non-blocking recv loop), set_busy_poll (SO_BUSY_POLL)
  src/framer.rs     4.7  Framer: one buffer, two cursors, the partial tail moved only when room runs short
  src/json.rs       4.7  DepthMsg / Level (borrowed, no allocation), DepthMsgOwned (six Strings), units()
  src/tls.rs        4.8  tls_feed: rustls in the reading thread (only with --features tls)
  src/hot.rs        4.10 open_hot: buffers before connect, Nagle off, SO_BUSY_POLL, non-blocking
  src/timing.rs     rdtsc behind lfence, calibrate(), hdrhistogram 1 ns..=1 s at 3 figures, the reports
  src/sys.rs        pinning, /proc counters (nstat's names), context switches, thread CPU time, the C0 hold
  src/counting.rs   the counting allocator of 4.7's "Seeing it"
  src/gen.rs        seeded depth updates (seed 7) in both encodings
  src/replay.rs     Lab 4.5's capture file: write_record, Capture, synthetic, import_hex
  src/bin/          ladder.rs  nagle.rs  pollwait.rs  parse.rs  replay.rs   (the lab section names them)
  tests/            no_alloc.rs   the chapter's counting-allocator check: a million updates through the Framer
                    json_allocs.rs  0, 0, 6 and 17 allocations a message
                    bins.rs       every binary once at a small size; ladder's and pollwait's --echo pairs over loopback
  runs/             each binary's output on the reference VM (see "Measured here")
```

Each module starts with its code block's own `use` lines. Where the chapter prints a block of statements, the module
wraps them in a function with the addresses made arguments, and changes nothing else; where it prints a loop that never
ends, one glue line (`if !more() { return Ok(()) }`) lets a test stop it.

## The labs

Every binary prints, first, the chapter's prediction for the variant asked for ("Predict first"), then count, p50, p99,
p99.9 and max, never a mean. `--help` on each lists every flag. Build with `cargo build --release -p ch04-network`;
the binaries are in `target/release/`.

| Lab | Command, as the lab section writes it | Here, on one box |
|---|---|---|
| 4.1 the ladder | `ladder --threads`, `--shm`, `--unix`, `--tcp-loopback`; `ladder --tcp <host>` against `ladder --echo` on the second machine, `--spin` or `--busy-poll` at both ends | `ladder --tcp-loopback --cores 0,1` |
| 4.1 bypass rung | `onload --profile=latency ./ladder --tcp <host>` against `onload --profile=latency ./ladder --echo` | needs Onload-supported cards |
| 4.2 Nagle | `nagle --loopback` and `nagle --host <host>` (against `nagle --echo`), default, `--nodelay`, `--one-write`, `--cork` | `nagle --loopback --one-write` |
| 4.3 poll vs wait | `pollwait --echo` on the second machine; the client `--blocking`, `--epoll`, `--spin`, `--busy-poll`, each with and without `--c0`, at `--gap 2000`, pinned with `--core <n>`; then `--blocking` without `--c0` at `--gap 0` | `pollwait --loopback --epoll --gap 2000 --core 0 --server-core 1` |
| 4.4 parse it | `parse --binary`, `--json`, `--json-borrowed`, `--json-value`, under `perf stat -e cycles,instructions,branches,branch-misses`, each count divided by 11 million | `parse --binary` |
| 4.5 capture and replay | `replay --file <path> --full-speed`, `replay --file <path> --paced` | `replay --write-synthetic min.rec`, then `--file min.rec` |

Defaults follow the chapter: the ladder a million round trips after 100,000 of warm-up; Nagle 1,000 requests after
100; poll vs wait 100,000 round trips (after 10,000); parse 10 million after a million. Ports: ladder 9000 (the
chapter's), nagle 9001, pollwait 9002, so all three servers can run on one second machine. Cores: the ladder's client
and echo on 2 and 3, pollwait's client on 3 (4.10's feed thread) and its forked server on 2, parse and replay on 2
(4.7's feed thread); a core that does not exist is a message and an unpinned run, and `none` asks for one.

What this crate adds to the lab's flags, so that each lab also runs on one box: `--loopback` on `pollwait` (forks the
server, as `nagle --loopback` does); `--connect host:port` and `--listen [addr:]port` as other names for the ladder's
`--tcp` and `--echo`; `--quickack` on Nagle's server (the pitfall that shows whose timer it is); and on `replay`,
`--write-synthetic` (a stand-in minute when there is no venue to capture), `--import-hex` (tshark's output as a
capture) and `--parser`. Pollwait carries `--gap` in each message, so one standalone server serves every client run.

### Two machines

On the second machine: `ladder --echo [--spin | --busy-poll]`, `nagle --echo`, `pollwait --echo [--tcp]`. On the first:
`ladder --tcp <host>`, `nagle --host <host>`, `pollwait --host <host> <mode>`. The chapter's rules for these runs: pin
the hot threads and stop irqbalance on both machines, say so in the write-up; record the card's driver, version and
PCIe address (`ethtool -i <iface>`), its model (`lspci -s <bus-info>`), its coalescing (`ethtool -c`), the cell of
`/proc/interrupts` that moved during the run, `uname -r`, the number of switches between the boxes and the p50 of a
thousand pings from the per-packet times. Turn coalescing off on both cards (`ethtool -C <iface> adaptive-rx off
rx-usecs 0`) and steer the test queue's interrupt to a core sharing the hot thread's L3 (blocking and `--spin`) or onto
the hot thread's own core (`--busy-poll`).

### Counting system calls and cycles

Count every variant's calls in a run of its own, never a timed one: `strace -c -f ./target/release/<bin> ...`; a
spinning variant's empty polls with `perf stat -e syscalls:sys_enter_recvfrom`. The chapter's counts per round trip:
ladder none for `--threads`/`--shm`, a send and a receive a side blocking, a send, one receive with data and tens to a
hundred EAGAIN spinning; Nagle two `sendto` and one `recvfrom` (default, `--nodelay`), one `writev` and one `recvfrom`
(`--one-write`), two `setsockopt`, two `sendto`, one `recvfrom` (`--cork`); pollwait one `recvfrom` (`--blocking`), one
`epoll_wait` and two `recvfrom`, one EAGAIN (`--epoll`), only `recvfrom` (spinning). The binaries print what they can
count themselves: empty polls, voluntary context switches, thread CPU time, and the changes in `TcpExtDelayedACKs`,
`TcpExtTCPAutoCorking`, `TcpOutSegs`, `TcpExtBusyPollRxPackets` and `UdpRcvbufErrors` (from /proc/net, box-wide).

Lab 4.4 under perf: `perf stat -e cycles,instructions,branches,branch-misses ./target/release/parse --binary`, then
`--json`, `--json-borrowed`, `--json-value`; divide each count by 11,000,000 (the binary prints the exact divisor).
`perf record -g ./target/release/parse --json && perf report` shows `parse_str`, the derived `Deserialize`,
`parse_integer`, and with owned strings `malloc`, `free`, `memcpy`. The binary path's code:
`objdump -d -M intel -C target/release/parse` at `ch04_network::read_fields` (or `cargo asm`), saved in
`runs/read_fields.asm.txt`: two compares, two branches, a handful of movs, price and qty in one `movups`.

### Lab 4.5: capturing a real feed

The replay half is `replay`; the capture half needs a venue. A record is an 8-byte little-endian timestamp in ns, a
4-byte length and the payload, one per WebSocket message after the decrypt.

1. The simpler way: in the feed client, after the decrypt, call `ch04_network::replay::write_record(&mut file, ts_ns,
   payload)` with the time its recv returned, for 60 s.
2. The way that keeps the wire's timing: run the client with `SSLKEYLOGFILE=keys.log` (rustls writes it once the
   config's `key_log` is `Arc::new(KeyLogFile::new())`; `tls::tls_feed(.., key_log: true, ..)` does that); start
   `tcpdump -i <iface> -B 65536 -w feed.pcap host <venue>` *before* the client connects, since a capture that misses
   the handshake cannot be decrypted; stop it after 60 s and check it reported no drops; then
   `tshark -r feed.pcap -o tls.keylog_file:keys.log -Y websocket -T fields -e frame.time_epoch -e websocket.payload > feed.txt`
   (payload field names vary by Wireshark version: `tshark -G fields | grep websocket`), and
   `replay --import-hex feed.txt --out feed.rec`. Several frames in one packet share its time: a clump.
   Delete the key log afterwards; it unlocks every session it lists. If the venue negotiated permessage-deflate,
   inflate the frames first and time that separately.

Then `replay --file feed.rec --full-speed` and `--paced`. If the venue offers a binary encoding of the same stream,
capture both at once on two connections and replay each. The replay leaves out the decrypt: charge each record
300–450 ns fixed plus 0.15–0.35 ns a byte, checked with `openssl speed -evp aes-128-gcm -decrypt -bytes <median record
size>` (the binary prints the arithmetic for the file's median).

## What needs root, perf, tcpdump or a second machine

- **Two machines**: the `--tcp <host>`/`--host <host>` rungs; every two-machine number in the chapter. Over loopback
  there is no card, no interrupt and no switch, so the ladder's two-machine rows and pollwait's mode differences cannot
  be reproduced on one box.
- **SO_BUSY_POLL** (`--busy-poll`, `open_hot`): before kernel 6.4, raising it above `net.core.busy_read` needs
  CAP_NET_ADMIN, and those kernels return EPERM; the binaries then print why, how to fix it (root, or `sysctl -w
  net.core.busy_read=50`) and that the run measures `--spin`. Since 6.4 anyone may set it. `open_hot` also sets
  `SO_PREFER_BUSY_POLL`, which needs CAP_NET_ADMIN on every kernel, and returns EPERM without it.
  On loopback it changes nothing: there is no card's ring to poll, and `TcpExtBusyPollRxPackets` stays at zero.
- **`--c0`** (pollwait): writes 0 to `/dev/cpu_dma_latency`, root on a stock box; refused, the run says so and goes on
  without it.
- **Pinning**: a core outside the process's affinity mask is a message and an unpinned run.
- **perf, tcpdump, tshark, bpftrace, turbostat, ethtool, Onload**: not on the reference VM; their commands are in each
  binary's `--help` and here. `strace` is.
- **The TLS client** (`--features tls`): builds rustls and its default crypto provider (aws-lc-rs, which needs a C
  compiler and cmake); it is never run here, there being no venue.

## Seeing it, per section

- 4.3 receive: `/proc/interrupts` (rows per queue, read twice), `ethtool -S <iface>` (`rx_missed_errors` or
  `rx_out_of_buffer`), `/proc/net/softnet_stat` (processed, dropped, time squeezes), `perf trace -e 'net:*' -e
  napi:napi_poll`, `bpftrace -e 'tracepoint:net:netif_receive_skb { @[cpu] = hist(args->len); }'`, `ethtool -T`,
  SO_TIMESTAMPING with `phc2sys`, `tcpdump -ttt -j adapter_unsynced`.
- 4.4 send: `ss -ti` (`ato:40`, `lastsnd`, `unacked`, `notsent`), `tcpdump -ttt -nn -i <iface> host <h> and port <p>`,
  `nstat -az` (`TcpExtDelayedACKs`, `TcpExtTCPAutoCorking`), `tc -s qdisc show dev <iface>`.
- 4.5 delivery: `ss -ti` (`lost`, `retrans`, `rto`, `rcv_ooopack`), `nstat -az TcpExtTCPLossProbes TcpRetransSegs
  TcpExtTCPFastRetrans TcpExtTCPTimeouts TcpExtTCPOFOQueue UdpRcvbufErrors`, `tcpdump -ni eth0 -ttt`.
- 4.6 waiting: `strace -c -f -p <pid>` or `perf trace -s -p <pid>`, `voluntary_ctxt_switches` in
  `/proc/<pid>/task/<tid>/status`, `perf stat -e context-switches,cpu-migrations -t <tid>`, `nstat -az
  TcpExtBusyPollRxPackets`.
- 4.8 TLS: `openssl speed -evp aes-128-gcm`, `-bytes 200 -decrypt`, `-evp chacha20-poly1305`; `grep -m1 -o -w aes
  /proc/cpuinfo` (and `pclmulqdq`; both are present on the reference VM); `ss -ti`, `/proc/net/tls_stat`.
- 4.9 bypass: `dpdk-devbind.py --status`, `onload_stackdump lots`, `ip link show dev <iface>` (`xdp` or `xdpgeneric`).

## Check

The chapter has no "Check:" paragraph; its check is each lab's "Write down" list, restated here.

- **4.1**: p50, p99 and max for every rung, the p50 in cycles and its ratio to the rung before, and the system calls per
  round trip. Expected ratios: about 1 threads to processes, 25–150 up to a Unix socket, 2 up to loopback, 2 again to
  the second machine, then about 3 down by busy-polling and 2.5 more by bypass; the blocking rungs' p99 far above their
  p50, the spinning rungs' p99 within a few microseconds of theirs.
- **4.2**: the total and the p50, p99 and max per request for each variant on each path; the system calls and segments
  per request; the changes in `TcpExtDelayedACKs` and `TcpExtTCPAutoCorking`. The default costs about 40 ms a request
  on loopback and across a switch alike; every fix, one ordinary round trip.
- **4.3**: for each mode with and without `--c0`, the p50, p99 and max, the thread's share of its core, the context
  switches and system calls per round trip; the empty polls a second for the spinning modes; the `--gap 0` run beside
  its twin.
- **4.4**: per message and mode, the nanoseconds, cycles, instructions, branches, branch misses and allocations (0, 0,
  6, 17), the ratio to `--binary` (a hundred or more for every JSON mode), and the price of an allocation and its free,
  (`--json` − `--json-borrowed`) / 6.
- **4.5**: the number of messages and their sizes, the full-speed rate in messages and megabytes a second, the p50, p99
  and max at the recorded pace, the largest clump, the records per message and the decrypt cost per record, and the
  same for the binary stream.

## Measured here

`runs/` holds each binary's output from the reference VM: 2 vCPUs of a Xeon at 2.8 GHz under KVM, kernel 6.18, run as
root, **with no isolated core (the binaries pin to cores 0 and 1 where asked, unpinned otherwise), no C-state or IRQ
tuning, and the box shared with other jobs** (load average 3–4 during the runs), loopback only. They show that every binary runs and what shape the numbers take; the
tails, in particular, are this box's neighbours, not the network.

What came out (p50 unless said; the full output, with p99, p99.9, max and the counters, is in `runs/`):

| Run | Here | The chapter's prediction |
|---|---|---|
| `ladder --threads` / `--shm` | 186 ns / 206 ns | 100–160 ns / 100–200 ns |
| `ladder --unix` / `--tcp-loopback`, pinned to cores 0 and 1 | 68 µs / 80 µs | 5–15 µs / 10–30 µs |
| the same, `--cores none` | 3.8 µs / 6.6 µs | |
| `ladder --unix --spin` / `--tcp-loopback --spin` / `--busy-poll` | 3.5 µs / 10.9 µs / 8.1 µs | (no loopback number) |
| `ladder --tcp 127.0.0.1` against `ladder --echo` | 82 µs | |
| `nagle --loopback`, default | 44.0 ms a request, 1 `TcpExtDelayedACKs` each | about 40 ms |
| `--nodelay` / `--one-write` / `--cork` / server `--quickack` | 10.3 / 6.8 / 7.3 / 11.5 µs | one round trip, 10–30 µs |
| `pollwait --loopback --gap 2000`: `--blocking` / `--epoll` | 80 µs / 110 µs, 1 voluntary switch a round trip, 2–3% CPU | 105–210 µs without `--c0` |
| `--spin` / `--busy-poll` at `--gap 2000` | 8 ms, 32–36% CPU, 0.8–0.9 involuntary switches a round trip | 8–15 µs, 100% CPU |
| `--blocking --gap 0` | 49 µs | near the `--c0` numbers |
| `parse --binary` / `--json-borrowed` / `--json` / `--json-value` | 13.3 / 424 / 643 / 2,023 ns; 0 / 0 / 6 / 17 allocations | 10–20 ns; under 1 µs, about 1 µs, 1–3 µs; 0 / 0 / 6 / 17 |
| `replay --full-speed`, a JSON minute / a binary minute | 120,000 in 83 ms (1.4 M a second, 280 MB/s), 439 ns each; 19 ns each | well under a second; 10–20 ns |
| `replay --paced`, JSON / binary | 4.4 µs / 845 ns | one parse: 1.5 µs / 16 ns |

What the numbers say about this box rather than the network:

- The blocking rungs pinned to the box's two cores are 5–10 times the chapter's, and unpinned they are at or below
  it. With other jobs on both cores, a sleeper pinned to its own core waits for whatever runs there to give the core
  up; unpinned, the scheduler wakes it beside its partner. Pinning without isolation is the stock box's worst case.
- `--busy-poll` on loopback is `--spin` (`TcpExtBusyPollRxPackets` +0): loopback has no card's ring to poll. The
  setsockopt itself succeeded, even as an unprivileged user: kernels since 6.4 do not ask for CAP_NET_ADMIN.
- The spinning pollwait modes at `--gap 2000` show the chapter's second caveat, not its numbers: client and server
  each spin on one of two cores that other jobs also want, get a third of a core each, and every reply waits out a
  time slice. `--c0` did not help (80 µs against 114 µs for `--blocking`, noise on this box): a guest's C-states
  are the hypervisor's.
- `parse`: the borrowed JSON parse is 32 times the binary read here, not the hundred the chapter expects, because the
  loop is as warm as the chapter warns; `--json-value` is 152 times. (`--json` − `--json-borrowed`) / 6 = 37 ns, one
  allocation and its free.
- At the recorded pace a parse is several times its full-speed cost: after half a millisecond of spinning, its caches
  and predictor are cold (the paced JSON p50 grows with the gap: 0.8 µs at a million frames a second, 4–8 µs at 2,000).
- Every p99.9 and max in the milliseconds is a time slice lost to a neighbour.

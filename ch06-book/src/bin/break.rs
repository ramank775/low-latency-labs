//! break: exercise 3. Drop one update, the delete of the best bid, and show
//! the copy diverging from a `TreeBook` that saw the whole stream, until an ask
//! arrives at or below the price that should be gone and the copy is crossed.
//! Then the sequence check in the feed handler that calls `apply`, with the
//! Stale state: the gap is seen on the next update, the book goes Stale, a
//! snapshot is requested and arrives 70 ms later (simulated, on the stream's
//! own clock), the buffered deltas are replayed, the state returns to Live and
//! the tops agree again; reported as the time in each state and the updates
//! buffered while Syncing, over several resyncs.

use ch05_structures::lab::{self, count, fmt_count, fmt_ns, now, Clock};
use ch06_book::{crossed, gen, ArrayBook, Book, BookState, FeedHandler, Side, Ticks, TreeBook, Update, VecBook, LEVELS};

const HELP: &str = "\
break: exercise 3, one dropped update, a crossed copy, and the resync that mends it

usage: break [--records N] [--seed S] [--rate R] [--snapshot-ms MS] [--resyncs K]
             [--book tree|vec] [--after N]

  --records N    updates in the stream (default 200,000)
  --seed S       the generator's seed (default 7)
  --rate R       updates a second on the stream's clock (default 20,000: one every 50 µs)
  --snapshot-ms MS  the snapshot's round trip, request to arrival (default 70); the venue
                 takes it half-way
  --resyncs K    updates dropped, so resyncs run, in part 2 (default 10)
  --book B       the handler's book: tree (default) or vec; on_snapshot starts from
                 B::default(), which the dense array, needing a base, does not have
  --after N      the first drop comes at least N updates after the opening (default 1,000)";

/// p50, p99 and max of a few samples, from a sorted Vec.
fn pct(v: &mut [f64]) -> (f64, f64, f64, usize) {
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1];
    if v.is_empty() { (0.0, 0.0, 0.0, 0) } else { (at(0.50), at(0.99), v[v.len() - 1], v.len()) }
}

fn line(name: &str, v: &mut [f64], unit: &str, scale: f64) {
    let (p50, p99, max, n) = pct(v);
    println!("    {name:<34} p50 {:>9} p99 {:>9} max {:>9} {unit}  (n {n})", fmt_ns(p50 / scale), fmt_ns(p99 / scale), fmt_ns(max / scale));
}

struct Pending { take_ns: u64, deliver_ns: u64, snap: Option<(u64, Vec<Update>)> }

#[derive(Default, Clone, Copy)]
struct Episode { drop_ns: u64, gap_ns: u64, request_ns: u64, joined_ns: u64, buffered: usize, join_ticks: u64, requests: u32 }

fn main() {
    let (mut records, mut seed, mut rate, mut snap_ms, mut resyncs, mut book, mut after) =
        (200_000u64, 7u64, 20_000u64, 70u64, 10u64, String::from("tree"), 1_000u64);
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let n = |args: &mut std::iter::Skip<std::env::Args>| count(&lab::value(args, &a, HELP)).unwrap_or_else(|| lab::usage(&format!("{a}: a count"), HELP));
        match a.as_str() {
            "--records" => records = n(&mut args),
            "--seed" => seed = n(&mut args),
            "--rate" => rate = n(&mut args).max(1),
            "--snapshot-ms" => snap_ms = n(&mut args),
            "--resyncs" => resyncs = n(&mut args).max(1),
            "--after" => after = n(&mut args),
            "--book" => book = lab::value(&mut args, &a, HELP),
            "-h" | "--help" => lab::usage("", HELP),
            other => lab::usage(&format!("unknown argument {other}"), HELP),
        }
    }

    lab::predict(&[
        "after the drop the copy keeps a best bid the venue removed: wrong from the next update on,",
        "silently, until an ask at or below it crosses the copy. With the check, the gap is seen on the",
        "next update, about 50 µs later at 20,000 a second; a 70 ms resync on the open connection buffers",
        "about 1,400 updates, one on a new connection, up to 350 ms, 7,000, more than the 4,096 reserved,",
        "so it starts again; the replay of them is the small part, the round trip the large one; and the",
        "tops agree again once the state is Live.",
    ]);
    let clock = Clock::new();
    let (updates, made) = gen::generate(&gen::Params { records, seed, ..Default::default() });
    let dt = 1e9 / rate as f64;
    let at = |k: usize| (k as f64 * dt) as u64;                      // update k's time on the stream's clock
    let ms = |ns: u64| ns as f64 / 1e6;
    println!("{}", lab::machine());
    println!("{} updates, seed {seed}, one every {:.0} µs ({} a second)\n", fmt_count(updates.len() as u64), dt / 1e3, fmt_count(rate));

    // ---------------------------------------------------------------- 1. drop one update
    // The drop: a delete of the best bid after which, as the chapter asks, an ask
    // arrives at or below the price before anything rewrites that bid; failing
    // that, the first delete of the best bid.
    let start = (made.opening + after) as usize;
    let mut venue = TreeBook::default();
    let (mut first, mut crossing) = (None, None);
    for (k, u) in updates.iter().enumerate() {
        if k >= start && u.side == Side::Bid && u.qty == 0 && venue.best_bid().is_some_and(|b| b.0 == u.price) {
            first = first.or(Some(k));
            let next = updates[k + 1..].iter().find(|v| (v.side == Side::Ask && v.qty > 0 && v.price <= u.price)
                                                      || (v.side == Side::Bid && v.price == u.price));
            if next.is_some_and(|v| v.side == Side::Ask) { crossing = Some(k); break; }
        }
        venue.apply(u);
    }
    let Some(dk) = crossing.or(first) else { lab::usage("no delete of the best bid after --after: use more --records", HELP) };
    let gone: Ticks = updates[dk].price;
    let mid = gen::first_mid(&updates).unwrap_or(gone);
    let (mut copy, mut venue) = (ArrayBook::new(mid - (LEVELS / 2) as Ticks), TreeBook::default());
    let (mut first_diff, mut diffs, mut cross, mut last_diff) = (None, 0u64, None, None);
    for (k, u) in updates.iter().enumerate() {
        venue.apply(u);
        if k == dk { continue; }                                   // lost on the way
        copy.apply(u);                                             // no check: whatever arrives is applied
        if k >= dk && copy.top() != venue.top() {
            diffs += 1;
            first_diff = first_diff.or(Some(u.seq));
            last_diff = Some(u.seq);
        }
        if k > dk && cross.is_none() && crossed(&copy) { cross = Some((u.seq, copy.top())); }
    }
    let dseq = updates[dk].seq;
    println!("1. Drop one update: seq {dseq}, the delete of the best bid at {gone} (${:.2}).", gone as f64 / 100.0);
    match first_diff {
        Some(s) => println!("   the copy's top differs from the venue's from seq {s} on: it still shows {gone} as its best bid"),
        None => println!("   the copy's top never differed: a later update rewrote {gone}"),
    }
    match cross {
        Some((s, top)) => println!("   an ask at or below {gone} arrived at seq {s}, {} updates and {:.2} ms after the drop: the copy is crossed, {:?}",
                                   s - dseq, ms(at((s - 1) as usize) - at(dk)), top),
        None => println!("   no ask reached {gone} before the stream ended: never crossed, just wrong"),
    }
    let left = updates.len() as u64 - dseq;
    println!("   from the drop to the end, {} of {} updates left the copy's top wrong{}; nothing in the book said so.\n",
             fmt_count(diffs), fmt_count(left),
             last_diff.map_or(String::new(), |l| if l < updates.len() as u64 { format!(", the last at seq {l}, when later updates happened to rewrite it") } else { String::new() }));

    // ---------------------------------------------------------------- 2. the check, Stale, and the resync
    let snap_ns = snap_ms * 1_000_000;
    let spacing = (updates.len() - dk) / resyncs as usize;
    let need = (snap_ns as f64 / dt) as usize + 2;
    let resyncs = if spacing > 2 * need { resyncs as usize } else { ((updates.len() - dk) / (2 * need + 1)).max(1) };
    let spacing = (updates.len() - dk) / resyncs;
    let drops: Vec<usize> = (0..resyncs).map(|i| dk + i * spacing).collect();
    println!("2. The sequence check in the feed handler, the Stale state, and the resync: {} dropped updates, \
              {} apart; the snapshot arrives {snap_ms} ms after the request, the venue taking it half-way; \
              the handler's book is {}.", drops.len(), fmt_count(spacing as u64), if book == "vec" { "VecBook" } else { "TreeBook" });
    let (eps, live_wrong, blind) = if book == "vec" {
        resync(FeedHandler { book: VecBook::default(), state: BookState::Live, next_seq: 1, buffer: Vec::with_capacity(4096) }, &updates, &drops, snap_ns, &at)
    } else {
        resync(FeedHandler { book: TreeBook::default(), state: BookState::Live, next_seq: 1, buffer: Vec::with_capacity(4096) }, &updates, &drops, snap_ns, &at)
    };
    for (i, e) in eps.iter().enumerate().take(3) {
        println!("   resync {}: dropped at {:.3} ms; gap seen {:.0} µs later: Live -> Stale; request sent at once: Stale -> Syncing;",
                 i + 1, ms(e.drop_ns), (e.gap_ns - e.drop_ns) as f64 / 1e3);
        println!("     snapshot joined {:.2} ms later with {} buffered deltas, the join itself {} µs of CPU{}: Syncing -> Live",
                 ms(e.joined_ns - e.request_ns), fmt_count(e.buffered as u64), fmt_ns(clock.ns(e.join_ticks) as f64 / 1e3),
                 if e.requests > 1 { format!(", after {} requests", e.requests) } else { String::new() });
    }
    if eps.len() > 3 { println!("   ... {} more", eps.len() - 3); }
    let col = |f: &dyn Fn(&Episode) -> f64| eps.iter().filter(|e| e.joined_ns > 0).map(f).collect::<Vec<f64>>();
    println!("   over {} resyncs:", eps.iter().filter(|e| e.joined_ns > 0).count());
    line("Live but wrong (drop to gap seen)", &mut col(&|e| (e.gap_ns - e.drop_ns) as f64), "µs", 1e3);
    line("Stale (gap seen to request sent)", &mut col(&|e| (e.request_ns - e.gap_ns) as f64), "µs", 1e3);
    line("Syncing (request to Live)", &mut col(&|e| (e.joined_ns - e.request_ns) as f64), "ms", 1e6);
    line("updates buffered while Syncing", &mut col(&|e| e.buffered as f64), "updates", 1.0);
    line("on_snapshot: load and replay, CPU", &mut col(&|e| clock.ns(e.join_ticks) as f64), "µs", 1e3);
    let total = updates.len() - drops.len();
    println!("   blind (not Live): {} of {} updates, {:.2}%; tops differing from the venue's while Live: {}",
             fmt_count(blind as u64), fmt_count(total as u64), 100.0 * blind as f64 / total as f64, live_wrong);
}

/// Part 2's loop: the stream on its own clock, the drops never arriving, the
/// handler's state driving the requests, the venue's book taking the snapshot.
fn resync<B: Book + Default>(mut h: FeedHandler<B>, updates: &[Update], drops: &[usize], snap_ns: u64,
                             at: &dyn Fn(usize) -> u64) -> (Vec<Episode>, u64, usize) {
    let mut venue = TreeBook::default();
    let (mut pending, mut eps, mut cur) = (None::<Pending>, Vec::<Episode>::new(), Episode::default());
    let (mut live_wrong, mut blind, mut next_drop) = (0u64, 0usize, 0usize);
    for (k, u) in updates.iter().enumerate() {
        let t = at(k);
        if let Some(p) = pending.as_mut() {
            if p.snap.is_none() && t >= p.take_ns { p.snap = Some((k as u64, venue.snapshot())); } // seqs 1..=k
        }
        if pending.as_ref().is_some_and(|p| t >= p.deliver_ns && p.snap.is_some()) {
            let p = pending.take().expect("pending");
            let (id, levels) = p.snap.expect("taken");
            cur.buffered = h.buffer.len();
            let t0 = now();
            let joined = h.on_snapshot(id, &levels);
            cur.join_ticks = now() - t0;
            if joined.is_some() { cur.joined_ns = p.deliver_ns; eps.push(cur); }
        }
        venue.apply(u);                                            // the venue saw it, lost or not
        if next_drop < drops.len() && k == drops[next_drop] {
            next_drop += 1;
            if h.state == BookState::Live { cur = Episode { drop_ns: t, ..Default::default() }; }
            continue;                                              // never arrives
        }
        let was_live = h.state == BookState::Live;
        h.on_update(*u);
        if h.state == BookState::Stale {                           // a gap, a cross, or a buffer full
            if was_live { cur.gap_ns = t; }
            if cur.requests == 0 { cur.request_ns = t; }           // the request goes out at once
            cur.requests += 1;
            let since_seq = h.buffer.first().map_or(u.seq, |b| b.seq);
            h.state = BookState::Syncing { since_seq };
            pending = Some(Pending { take_ns: t + snap_ns / 2, deliver_ns: t + snap_ns, snap: None });
        }
        if h.state == BookState::Live { live_wrong += (h.book.top() != venue.top()) as u64; } else { blind += 1; }
    }
    (eps, live_wrong, blind)
}

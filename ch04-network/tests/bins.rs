// tests/bins.rs: every lab binary, run once at a small size, must finish and
// report. The runs are serialised: spinning pairs on a two-core box would
// otherwise starve each other and turn seconds into minutes.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Mutex;

static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn run(bin: &str, args: &[&str]) -> String {
    let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let out = Command::new(bin).args(args).output().expect("the binary runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{bin} {args:?} failed:\n{text}");
    assert!(text.contains("Predict first") || args.contains(&"--write-synthetic") || args.contains(&"--import-hex"),
            "{bin} {args:?} printed no prediction:\n{text}");
    text
}

/// The p50 the report line prints, in ns.
fn p50_ns(text: &str) -> f64 {
    let after = text.split(" p50 ").nth(1).expect("a p50");
    let mut w = after.split_whitespace();
    let v: f64 = w.next().unwrap().parse().unwrap();
    match w.next().unwrap() { "ns" => v, "µs" => v * 1e3, "ms" => v * 1e6, u => panic!("unit {u}") }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

const LADDER: &str = env!("CARGO_BIN_EXE_ladder");
const NAGLE: &str = env!("CARGO_BIN_EXE_nagle");
const POLLWAIT: &str = env!("CARGO_BIN_EXE_pollwait");
const PARSE: &str = env!("CARGO_BIN_EXE_parse");
const REPLAY: &str = env!("CARGO_BIN_EXE_replay");

#[test]
fn ladder_local_rungs() {
    for (mode, iters) in [("--threads", "20000"), ("--shm", "20000"), ("--unix", "2000"), ("--tcp-loopback", "2000")] {
        let t = run(LADDER, &[mode, "--iters", iters, "--warmup", "200", "--cores", "none"]);
        assert!(t.contains("max") && t.contains("cycles at the chapter's 4 GHz"), "{t}");
    }
    let t = run(LADDER, &["--tcp-loopback", "--spin", "--iters", "300", "--warmup", "30", "--cores", "none"]);
    assert!(t.contains("empty polls"), "{t}");
    run(LADDER, &["--unix", "--busy-poll", "--iters", "300", "--warmup", "30", "--cores", "none"]);
}

#[test]
fn ladder_two_machine_pair_over_loopback() {
    let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let port = free_port().to_string();
    let mut echo = Command::new(LADDER).args(["--echo", "--port", &port, "--core", "none"])
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("ladder --echo");
    let mut first = String::new();
    BufReader::new(echo.stdout.as_mut().unwrap()).read_line(&mut first).unwrap();
    assert!(first.contains("listening"), "{first}");
    let out = Command::new(LADDER).args(["--tcp", &format!("127.0.0.1:{port}"), "--iters", "2000", "--warmup", "100",
                                         "--core", "none"]).output().unwrap();
    let _ = echo.kill();
    let _ = echo.wait();
    let t = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && t.contains("p99"), "{t}");
}

#[test]
fn nagle_stalls_by_default_and_not_with_one_write() {
    let stalled = run(NAGLE, &["--loopback", "--requests", "4", "--warmup", "4"]);
    let whole = run(NAGLE, &["--loopback", "--one-write", "--requests", "200", "--warmup", "20"]);
    run(NAGLE, &["--loopback", "--nodelay", "--requests", "50", "--warmup", "10"]);
    run(NAGLE, &["--loopback", "--cork", "--requests", "50", "--warmup", "10"]);
    // the chapter's 40 ms against one ordinary round trip: a factor of hundreds
    assert!(p50_ns(&stalled) > 20e6, "no delayed-ACK stall:\n{stalled}");
    assert!(p50_ns(&whole) < 2e6, "one write per message stalled:\n{whole}");
    let quick = run(NAGLE, &["--loopback", "--quickack", "--requests", "20", "--warmup", "4"]);
    assert!(p50_ns(&quick) < 20e6, "TCP_QUICKACK on the server should remove the stall:\n{quick}");
}

#[test]
fn pollwait_every_mode_over_loopback() {
    for mode in ["--blocking", "--epoll", "--spin", "--busy-poll"] {
        let t = run(POLLWAIT, &["--loopback", mode, "--iters", "300", "--warmup", "30", "--core", "none", "--server-core", "none"]);
        assert!(t.contains("client CPU") && t.contains("lost (100 ms timeout): 0"), "{t}");
    }
    run(POLLWAIT, &["--loopback", "--epoll", "--tcp", "--gap", "50", "--iters", "200", "--warmup", "20",
                    "--core", "none", "--server-core", "none"]);
}

#[test]
fn pollwait_client_against_a_standalone_echo() {
    let _g = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let port = free_port().to_string();
    let mut echo = Command::new(POLLWAIT).args(["--echo", "--port", &port, "--core", "none"])
        .stdout(Stdio::piped()).stderr(Stdio::null()).spawn().expect("pollwait --echo");
    let mut first = String::new();
    BufReader::new(echo.stdout.as_mut().unwrap()).read_line(&mut first).unwrap();
    let out = Command::new(POLLWAIT).args(["--host", &format!("127.0.0.1:{port}"), "--blocking", "--iters", "300",
                                           "--warmup", "30", "--core", "none"]).output().unwrap();
    let _ = echo.kill();
    let _ = echo.wait();
    let t = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && t.contains("p99"), "{first}\n{t}");
}

#[test]
fn parse_counts_the_chapters_allocations() {
    for (mode, allocs) in [("--binary", "0.00"), ("--json-borrowed", "0.00"), ("--json", "6.00"), ("--json-value", "17.00")] {
        let t = run(PARSE, &[mode, "--messages", "51200", "--warmup", "5000", "--core", "none"]);
        assert!(t.contains(&format!("allocations per message: {allocs}")), "{mode}:\n{t}");
    }
}

#[test]
fn replay_synthetic_full_speed_paced_and_import() {
    let dir = env!("CARGO_TARGET_TMPDIR");
    let json = format!("{dir}/ch04-replay-json.rec");
    let bin = format!("{dir}/ch04-replay-bin.rec");
    run(REPLAY, &["--write-synthetic", &json, "--messages", "4000", "--rate", "20000"]);
    run(REPLAY, &["--write-synthetic", &bin, "--encoding", "binary", "--messages", "4000", "--rate", "20000"]);
    let t = run(REPLAY, &["--file", &json, "--full-speed", "--core", "none"]);
    assert!(t.contains("parser json-borrowed") && t.contains("0 frames the parser refused"), "{t}");
    let t = run(REPLAY, &["--file", &bin, "--full-speed", "--core", "none"]);
    assert!(t.contains("parser binary") && t.contains("0 frames the parser refused"), "{t}");
    let t = run(REPLAY, &["--file", &json, "--paced", "--parser", "json", "--core", "none"]);
    assert!(t.contains("release to end of parse") && t.contains("0 frames the parser refused"), "{t}");
    let hex = format!("{dir}/ch04-replay.txt");
    let ex = r#"{"channel":"book","type":"depth_update","instrument":"BTC-USDT-PERP","data":{"sequence":1,"timestamp":2,"side":"ask","price":"68123.45000000","quantity":"0.51200000"}}"#;
    let h: String = ex.bytes().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&hex, format!("1792046400.000100000\t{h},{h}\n1792046400.000600000\t{h}\n")).unwrap();
    let imported = format!("{dir}/ch04-replay-imported.rec");
    run(REPLAY, &["--import-hex", &hex, "--out", &imported]);
    let t = run(REPLAY, &["--file", &imported, "--full-speed", "--core", "none"]);
    assert!(t.contains("3 frames") && t.contains("largest clump 2"), "{t}");
}

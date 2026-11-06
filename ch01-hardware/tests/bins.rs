// tests/bins.rs: every lab binary runs at a small size, prints its prediction
// first and its percentiles after, and answers --help.

use std::process::Command;

fn run(bin: &str, args: &[&str]) -> String {
    let out = Command::new(bin).args(args).output().expect("the binary runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{bin} {args:?} failed:\n{text}\n{}", String::from_utf8_lossy(&out.stderr));
    text
}

fn check(bin: &str, args: &[&str], must: &[&str]) {
    let text = run(bin, args);
    assert!(text.starts_with("Predict first (the chapter's prediction): "), "{bin}: prediction first:\n{text}");
    for m in must {
        assert!(text.contains(m), "{bin} {args:?}: no {m:?} in\n{text}");
    }
    assert!(!text.contains("ERROR"), "{bin}: {text}");
    let help = run(bin, &["--help"]);
    assert!(help.contains("usage:"), "{bin} --help:\n{help}");
}

#[test]
fn stride_runs() {
    check(env!("CARGO_BIN_EXE_stride"), &["--size", "2", "--reps", "3", "--core", "none"],
          &["p50", "p99", "max", "4096", "Write down"]);
}

#[test]
fn branch_runs() {
    check(env!("CARGO_BIN_EXE_branch"), &["--size", "1", "--reps", "3", "--core", "none"],
          &["unsorted", "sorted", "branchless", "vector", "p50", "unsorted / sorted"]);
    check(env!("CARGO_BIN_EXE_branch"), &["--size", "1", "--reps", "2", "--variant", "sorted", "--core", "none"],
          &["sorted"]);
}

#[test]
fn false_sharing_runs() {
    check(env!("CARGO_BIN_EXE_false_sharing"), &["--iters", "200000", "--block", "10000", "--cores", "none,none"],
          &["adjacent", "padded", "align64", "same line", "different lines", "adjacent / padded"]);
}

#[test]
fn chase_runs() {
    check(env!("CARGO_BIN_EXE_chase"), &["--nodes", "100000", "--reps", "2", "--core", "none"],
          &["vec", "list", "boxes", "list / vec", "p50"]);
    check(env!("CARGO_BIN_EXE_chase"), &["--nodes", "50000", "--reps", "1", "--no-shuffle", "--huge", "--core", "none"],
          &["allocation order"]);
}

#[test]
fn sum_and_layout_run() {
    check(env!("CARGO_BIN_EXE_sum"), &["--iters", "2000", "--core", "none"], &["sum1", "sum4", "p50"]);
    let text = run(env!("CARGO_BIN_EXE_layout"), &[]);
    assert!(text.contains("\n40 8\n24 8\n24 8\n"), "{text}");
}

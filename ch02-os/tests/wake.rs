// tests/wake.rs, a binary of its own: the allocation counter sees every thread.
use ch02_os::{allowed_cores, calibrate, ping_pong, ToNs, Wait};

/// Lab 2.4's three ways of passing the token: every round trip recorded,
/// nothing allocated during them, and sleep never faster than its two sleeps.
#[test]
fn every_variant_passes_the_token_and_allocates_nothing() {
    let ns = ToNs::new(calibrate());
    let cores = allowed_cores();
    // two different cores if the box has them; one-core boxes run both unpinned
    let pin = if cores.len() >= 2 { [Some(cores[0]), Some(cores[1])] } else { [None, None] };
    for (wait, iters) in [(Wait::Spin, 2_000), (Wait::Park, 500), (Wait::Sleep, 200)] {
        let r = ping_pong(wait, iters, 50, pin, None, ns);
        assert_eq!(r.h.len(), iters, "{wait:?}: round trips recorded");
        assert_eq!(r.allocs, 0, "{wait:?}: allocated during the round trips");
        assert!(r.notes.is_empty(), "{wait:?}: {:?}", r.notes);
        if wait == Wait::Sleep {
            assert!(r.h.value_at_quantile(0.5) >= 2_000, "two sleeps of at least 1 µs each");
        }
    }
    let r = ping_pong(Wait::Sleep, 50, 10, pin, Some(1), ns);
    assert_eq!(r.slack_ns, [1, 1], "PR_SET_TIMERSLACK on both threads");
}

//! VM starts that keep failing must not be tried again and again.

use cf_budget::Millis;
use cf_lifecycle::{BreakerLimits, StartBreaker};
use proptest::prelude::*;

fn s(seconds: u64) -> Millis {
    Millis::secs(seconds)
}

#[test]
fn three_failures_in_a_row_close_the_mac() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    assert!(breaker.admit(s(0)) && !breaker.closed());
    breaker.failed(s(1));
    breaker.failed(s(2));
    assert!(breaker.admit(s(2)) && !breaker.closed() && breaker.retry_at().is_none());
    breaker.failed(s(3));
    assert!(breaker.closed());
    assert_eq!(breaker.retry_at(), Some(s(33)));
    assert!(!breaker.admit(s(3)));
    assert!(!breaker.admit(Millis(32_999)));
}

#[test]
fn a_success_between_failures_starts_the_count_again() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    for second in 0..100 {
        breaker.failed(s(second));
        breaker.failed(s(second));
        breaker.succeeded();
        assert!(!breaker.closed() && breaker.admit(s(second)));
    }
}

#[test]
fn one_start_per_wait_and_the_wait_doubles_to_fifteen_minutes() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    let mut now = 0;
    for _ in 0..3 {
        breaker.failed(s(now));
    }
    let mut waits = Vec::new();
    for _ in 0..8 {
        let at = breaker.retry_at().unwrap();
        waits.push((at.0 - s(now).0) / 1000);
        assert!(!breaker.admit(Millis(at.0 - 1)));
        assert!(breaker.admit(at), "one start is let through");
        assert!(
            !breaker.admit(at) && !breaker.admit(at.plus(s(10_000))),
            "and only one until it reports back"
        );
        now = at.0 / 1000 + 20;
        breaker.failed(s(now));
        assert!(breaker.closed());
    }
    assert_eq!(waits, [30, 60, 120, 240, 480, 900, 900, 900]);
}

#[test]
fn the_single_start_working_opens_the_mac_and_forgets_the_wait() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    for round in 0..3 {
        let start = s(round * 10_000);
        for _ in 0..3 {
            breaker.failed(start);
        }
        assert!(breaker.admit(start.plus(s(30))));
        breaker.failed(start.plus(s(50)));
        assert_eq!(
            breaker.retry_at(),
            Some(start.plus(s(110))),
            "sixty seconds after the second failure"
        );
        assert!(breaker.admit(start.plus(s(110))));
        breaker.succeeded();
        assert!(!breaker.closed() && breaker.retry_at().is_none());
        assert!(breaker.admit(start.plus(s(111))) && breaker.admit(start.plus(s(111))));
    }
}

#[test]
fn a_start_that_never_happened_does_not_use_up_the_try() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    for _ in 0..3 {
        breaker.failed(s(0));
    }
    assert!(breaker.admit(s(30)));
    assert!(!breaker.admit(s(31)));
    breaker.unused();
    assert!(
        breaker.admit(s(31)),
        "the claim was refused, so nothing was started"
    );
    assert!(breaker.closed());
}

#[test]
fn failures_of_vms_started_before_it_closed_do_not_double_the_wait() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    for second in 0..9 {
        breaker.failed(s(second));
    }
    assert_eq!(
        breaker.retry_at(),
        Some(s(38)),
        "thirty seconds after the last of them"
    );
}

#[test]
fn odd_limits_still_close() {
    let mut breaker = StartBreaker::new(BreakerLimits {
        failures: 0,
        first_wait: s(5),
        longest_wait: s(1),
    });
    breaker.failed(s(0));
    assert_eq!(breaker.retry_at(), Some(s(5)));
    assert!(breaker.admit(s(5)));
    breaker.failed(s(6));
    assert_eq!(
        breaker.retry_at(),
        Some(s(7)),
        "never longer than the longest wait"
    );
    let mut breaker = StartBreaker::new(BreakerLimits {
        failures: 1,
        first_wait: Millis(u64::MAX),
        longest_wait: Millis(u64::MAX),
    });
    breaker.failed(s(9));
    assert_eq!(breaker.retry_at(), Some(Millis(u64::MAX)));
}

proptest! {
    /// The Go manager started a VM that could not start 2,400 times in a day. Whatever the Runner's callers do,
    /// a Mac whose VMs never start gets at most the three starts that close it, one per wait after that, and no
    /// second one while the first has not reported back.
    #[test]
    fn a_mac_whose_vms_never_start_is_tried_rarely(steps in prop::collection::vec((0..120_000u64, 0..4u8), 1..600)) {
        let limits = BreakerLimits::default();
        let mut breaker = StartBreaker::new(limits);
        let mut now = Millis(0);
        let mut running: Vec<Millis> = Vec::new();
        let mut starts_while_closed: Vec<Millis> = Vec::new();
        let mut starts_before = 0u32;
        for (wait, what) in steps {
            now = now.plus(Millis(wait));
            // Every VM that was started fails after twenty seconds.
            let (failed, alive): (Vec<Millis>, Vec<Millis>) = running.iter().partition(|at| now.since(**at) >= Millis::secs(20));
            running = alive;
            for _ in failed {
                breaker.failed(now);
            }
            let closed = breaker.closed();
            if what < 3 && breaker.admit(now) {
                if closed {
                    prop_assert!(running.is_empty() || starts_while_closed.is_empty(), "a second start while one is out");
                    if let Some(last) = starts_while_closed.last() {
                        prop_assert!(now.since(*last) >= limits.first_wait, "two starts within the shortest wait");
                    }
                    starts_while_closed.push(now);
                } else {
                    starts_before += 1;
                }
                running.push(now);
            }
        }
        let day = Millis::mins(24 * 60);
        let per_day = starts_while_closed.iter().filter(|at| at.0 < day.0).count();
        // 30 s + 60 s + ... + 8 min is 15.5 min for the first five; the rest of the day is one per 15 min.
        prop_assert!(per_day <= 5 + 24 * 4, "{} starts in a day", per_day);
        let _ = starts_before;
    }
}

//! VM starts that keep failing must not be tried again and again.

use cf_budget::{
    Assigned, Budget, GIB, Hold, HostFacts, HostReading, Millis, Mode, Now, Pressure, Refusal, Settings,
    Size, Thermal,
};
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
            !breaker.admit(at) && !breaker.admit(at.plus(s(20))),
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
fn a_try_that_never_reports_back_does_not_close_the_mac_for_good() {
    // The slot of the one try was stopped while its VM booted, or its job went to another Mac: neither a failure
    // nor a success is ever reported. One more wait, and the next try may go.
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    for _ in 0..3 {
        breaker.failed(s(0));
    }
    assert!(breaker.admit(s(30)));
    assert!(!breaker.admit(Millis(59_999)) && breaker.holds(Millis(59_999)));
    assert!(!breaker.holds(s(60)));
    assert!(breaker.admit(s(60)), "thirty seconds without a word of the try");
    assert!(!breaker.admit(s(61)) && breaker.closed());
    // The wait in force is what a try gets: after a failed try it is sixty seconds.
    breaker.failed(s(70));
    assert_eq!(breaker.retry_at(), Some(s(130)));
    assert!(breaker.admit(s(130)));
    assert!(!breaker.admit(Millis(189_999)));
    assert!(breaker.admit(s(190)));
    // A late word of a try that was given up on changes nothing about the next one.
    breaker.succeeded();
    assert!(!breaker.closed() && !breaker.holds(s(190)));
}

#[test]
fn the_hold_is_on_exactly_while_no_start_is_let_through() {
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    assert!(!breaker.holds(s(0)));
    breaker.failed(s(0));
    breaker.failed(s(0));
    assert!(!breaker.holds(s(0)), "two failures do not close");
    breaker.failed(s(0));
    for (at, held) in [(0, true), (29_999, true), (30_000, false), (500_000, false)] {
        assert_eq!(breaker.holds(Millis(at)), held, "at {at} ms");
        assert_eq!(breaker.clone().admit(Millis(at)), !held, "at {at} ms");
    }
    // Asking changes nothing; the try going out does.
    assert!(breaker.admit(s(30)));
    assert!(breaker.holds(s(30)) && breaker.holds(s(45)));
    breaker.unused();
    assert!(!breaker.holds(s(45)));
}

fn calm(second: u64) -> (Now, HostReading) {
    let at = Now {
        awake: s(second),
        elapsed: s(second),
    };
    let reading = HostReading {
        at,
        available_bytes: 16 * GIB,
        pressure: Pressure::Normal,
        swap_used_bytes: 0,
        swapouts: 0,
        idle: Millis::mins(60),
        thermal: Thermal::Nominal,
        on_battery: false,
        disk_free_bytes: 500 * GIB,
        disk_reclaimable_bytes: 0,
    };
    (at, reading)
}

#[test]
fn with_the_budget_a_closed_mac_tries_one_job_per_wait_and_says_why_it_takes_no_other() {
    // The daemon's loop as the breaker asks for it: the hold follows `holds`, every grant is told to `admit`.
    let facts = HostFacts {
        total_bytes: 16 * GIB,
        cores: 10,
    };
    let mut budget = Budget::new(Settings::for_host(facts, Mode::Dedicated)).unwrap();
    let mut breaker = StartBreaker::new(BreakerLimits::default());
    let mut granted = Vec::new();
    let mut waiting = None;
    for second in 0..=400 {
        let (now, reading) = calm(second);
        budget.hold(Hold::StartFailures, breaker.holds(now.elapsed));
        let observed = budget.observe(now, Some(reading));
        for grant in &observed.started {
            assert!(breaker.admit(now.elapsed), "a job started under the hold");
            granted.push((second, grant.slot));
        }
        match second {
            // Three VMs fail to start: the Mac closes.
            100 => (0..3).for_each(|_| breaker.failed(now.elapsed)),
            // GitHub assigns a job without asking while it is closed: it waits, and the status says for what.
            105 => {
                let Assigned::Waits { slot, why } = budget.assign(now, Size::G2) else {
                    panic!("started on a Mac whose VMs do not start")
                };
                assert_eq!(why, Refusal::Held(Hold::StartFailures));
                assert_eq!(budget.status(now).waiting[0].why, Some(why));
                waiting = Some(slot);
            }
            // The try fails after its twenty seconds, and its slot is given back.
            150 => {
                budget.release(granted[0].1).unwrap();
                breaker.failed(now.elapsed);
            }
            _ => {}
        }
        // The Runner asks for more work whenever it may; under the hold it is refused by name.
        budget.hold(Hold::StartFailures, breaker.holds(now.elapsed));
        if second >= 100 && breaker.admit(now.elapsed) {
            match budget.claim(now, Size::G2) {
                Ok(grant) => granted.push((second, grant.slot)),
                Err(_) => breaker.unused(),
            }
        } else if second >= 100 {
            assert_eq!(
                budget.claim(now, Size::G2),
                Err(Refusal::Held(Hold::StartFailures))
            );
        }
    }
    // Closed at 100. The first try at 130 is the job that waited, and nothing else until it has failed; the
    // next one sixty seconds after that failure, at 210, and it never reports back: sixty more, 270, 330, 390.
    assert_eq!(granted[0], (130, waiting.unwrap()));
    assert_eq!(
        granted.iter().map(|(second, _)| *second).collect::<Vec<_>>(),
        [130, 210, 270, 330, 390]
    );
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

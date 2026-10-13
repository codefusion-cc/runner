//! The two clocks the budget counts time by, as [`Now`]: `awake` stands still while the host sleeps or is
//! suspended, `elapsed` does not. Neither moves with the wall clock. Every `Host` of a real machine reads them here.

use cf_budget::{Millis, Now};

/// `CLOCK_MONOTONIC` stops while Linux is suspended, `CLOCK_BOOTTIME` counts on.
#[cfg(target_os = "linux")]
const CLOCKS: (libc::clockid_t, libc::clockid_t) = (libc::CLOCK_MONOTONIC, libc::CLOCK_BOOTTIME);
/// `CLOCK_UPTIME_RAW` stops while the Mac sleeps, `CLOCK_MONOTONIC_RAW` counts on.
#[cfg(not(target_os = "linux"))]
const CLOCKS: (libc::clockid_t, libc::clockid_t) = (libc::CLOCK_UPTIME_RAW, libc::CLOCK_MONOTONIC_RAW);

/// Both clocks now. A clock that cannot be read answers zero for both, which the budget takes as time gone
/// backwards, so it closes the host rather than trusting a time it does not have.
pub fn now() -> Now {
    match (read(CLOCKS.0), read(CLOCKS.1)) {
        (Some(awake), Some(elapsed)) => Now { awake, elapsed },
        _ => Now::default(),
    }
}

fn read(id: libc::clockid_t) -> Option<Millis> {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `time` is a timespec this call may write, and the call writes nothing else.
    let status = unsafe { libc::clock_gettime(id, &mut time) };
    let secs = u64::try_from(time.tv_sec).ok()?;
    let nanos = u64::try_from(time.tv_nsec).ok()?;
    (status == 0).then(|| Millis(secs.saturating_mul(1000).saturating_add(nanos / 1_000_000)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::now;

    #[test]
    fn both_clocks_go_on_with_the_time_and_the_awake_one_is_never_ahead() {
        let first = now();
        std::thread::sleep(Duration::from_millis(30));
        let second = now();
        assert!(first.awake.0 > 0 && first.elapsed.0 > 0, "{first:?}");
        // Each moved at least as far as the time slept, give or take the millisecond each is cut to.
        assert!(second.awake.0 + 1 >= first.awake.0 + 30, "{first:?} {second:?}");
        assert!(
            second.elapsed.0 + 1 >= first.elapsed.0 + 30,
            "{first:?} {second:?}"
        );
        assert!(second.awake <= second.elapsed, "{second:?}");
    }
}

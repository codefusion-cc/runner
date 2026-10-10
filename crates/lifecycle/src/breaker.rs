use cf_budget::Millis;

/// When VM starts that fail close the Mac, and how fast it tries again.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerLimits {
    /// So many starts failing in a row close the Mac.
    pub failures: u32,
    /// How long it stays closed before one start is tried.
    pub first_wait: Millis,
    /// The wait doubles with every further failure, up to this.
    pub longest_wait: Millis,
}

impl Default for BreakerLimits {
    /// Three failures in a row; then one try after 30 s, doubling to 15 minutes.
    fn default() -> Self {
        BreakerLimits {
            failures: 3,
            first_wait: Millis::secs(30),
            longest_wait: Millis::mins(15),
        }
    }
}

/// Counts VM starts that fail in a row and, past the limit, lets only one start through per wait.
///
/// How the daemon drives it:
///
/// - Before every reading, claim and assigned job, `budget.hold(Hold::StartFailures, breaker.holds(now))`. The
///   hold keeps every job out, one that GitHub assigned without asking too, and the console shows the reason. It
///   is off for the moment of the one try, or the try could never be granted.
/// - Ask [`StartBreaker::admit`] before every claim, and call it for every grant the budget hands out by itself
///   (`Observed::started`, `Assigned::Start`): it notes that the try is out, and the hold comes back.
/// - Report every [`crate::Effect::StartFailed`] and [`crate::Effect::StartSucceeded`].
///
/// A try that never reports back (its slot was stopped while the VM booted, or its job went elsewhere) does not
/// close the Mac for good: after one more wait it counts as not made. [`StartBreaker::unused`] says so sooner.
#[derive(Clone, Debug)]
pub struct StartBreaker {
    limits: BreakerLimits,
    failed_in_a_row: u32,
    wait: Millis,
    /// `None` while healthy.
    closed: Option<Closed>,
}

#[derive(Clone, Copy, Debug)]
struct Closed {
    /// When the next single start may be tried.
    retry_at: Millis,
    /// Since when a single start is out that has not reported back.
    trying: Option<Millis>,
}

impl StartBreaker {
    /// A breaker with nothing failed. A limit of zero failures counts as one.
    pub fn new(limits: BreakerLimits) -> StartBreaker {
        StartBreaker {
            limits,
            failed_in_a_row: 0,
            wait: limits.first_wait,
            closed: None,
        }
    }

    /// Whether a slot may be claimed now. Healthy: always. Closed: once per wait, and not again until that start
    /// has reported back, [`StartBreaker::unused`] says it never happened, or a whole wait passed without a word
    /// of it.
    pub fn admit(&mut self, now: Millis) -> bool {
        if self.holds(now) {
            return false;
        }
        if let Some(closed) = &mut self.closed {
            closed.trying = Some(now);
        }
        true
    }

    /// Whether `Hold::StartFailures` belongs on the budget at `now`: starts failed too often, and it is not the
    /// moment of the one try. Exactly then [`StartBreaker::admit`] refuses.
    pub fn holds(&self, now: Millis) -> bool {
        self.closed.is_some_and(|closed| {
            let try_is_out = closed.trying.is_some_and(|since| now.since(since) < self.wait);
            now < closed.retry_at || try_is_out
        })
    }

    /// The start that [`StartBreaker::admit`] let through did not happen (the claim was refused, or another Mac
    /// got the job): the next one may be tried.
    pub fn unused(&mut self) {
        if let Some(closed) = &mut self.closed {
            closed.trying = None;
        }
    }

    /// A VM did not reach `ready`.
    pub fn failed(&mut self, now: Millis) {
        self.failed_in_a_row = self.failed_in_a_row.saturating_add(1);
        if self.failed_in_a_row < self.limits.failures.max(1) {
            return;
        }
        if self.closed.is_some_and(|closed| closed.trying.is_some()) {
            self.wait = self.wait.plus(self.wait).min(self.limits.longest_wait);
        }
        self.closed = Some(Closed {
            retry_at: now.plus(self.wait),
            trying: None,
        });
    }

    /// A VM reached `ready`: whatever was wrong is over.
    pub fn succeeded(&mut self) {
        *self = StartBreaker::new(self.limits);
    }

    /// Whether starts failed so often that the Mac takes no jobs but the single tries.
    pub fn closed(&self) -> bool {
        self.closed.is_some()
    }

    /// When the next single start may be tried; `None` while healthy.
    pub fn retry_at(&self) -> Option<Millis> {
        self.closed.map(|closed| closed.retry_at)
    }
}

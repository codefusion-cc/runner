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
/// Ask [`StartBreaker::admit`] before every claim of a slot. Report every [`crate::Effect::StartFailed`] and
/// [`crate::Effect::StartSucceeded`]. While [`StartBreaker::closed`] is true, set `Hold::StartFailures` on the
/// budget, so the reason shows in the console.
#[derive(Clone, Debug)]
pub struct StartBreaker {
    limits: BreakerLimits,
    failed_in_a_row: u32,
    wait: Millis,
    /// When the next single start may be tried; `None` while healthy.
    retry_at: Option<Millis>,
    /// A single start was let through and has not reported back.
    trying: bool,
}

impl StartBreaker {
    /// A breaker with nothing failed. A limit of zero failures counts as one.
    pub fn new(limits: BreakerLimits) -> StartBreaker {
        StartBreaker {
            limits,
            failed_in_a_row: 0,
            wait: limits.first_wait,
            retry_at: None,
            trying: false,
        }
    }

    /// Whether a slot may be claimed now. Healthy: always. Closed: once per wait, and not again until that start
    /// has reported back or [`StartBreaker::unused`] says it never happened.
    pub fn admit(&mut self, now: Millis) -> bool {
        match self.retry_at {
            None => true,
            Some(at) if now >= at && !self.trying => {
                self.trying = true;
                true
            }
            Some(_) => false,
        }
    }

    /// The start that [`StartBreaker::admit`] let through did not happen (the claim was refused, or another Mac
    /// got the job): the next one may be tried.
    pub fn unused(&mut self) {
        self.trying = false;
    }

    /// A VM did not reach `ready`.
    pub fn failed(&mut self, now: Millis) {
        self.failed_in_a_row = self.failed_in_a_row.saturating_add(1);
        if self.failed_in_a_row < self.limits.failures.max(1) {
            return;
        }
        if self.retry_at.is_some() && self.trying {
            self.wait = Millis(self.wait.0.saturating_mul(2)).min(self.limits.longest_wait);
        }
        self.trying = false;
        self.retry_at = Some(now.plus(self.wait));
    }

    /// A VM reached `ready`: whatever was wrong is over.
    pub fn succeeded(&mut self) {
        self.failed_in_a_row = 0;
        self.wait = self.limits.first_wait;
        self.retry_at = None;
        self.trying = false;
    }

    /// Whether starts failed so often that the Mac takes no jobs but the single tries.
    pub fn closed(&self) -> bool {
        self.retry_at.is_some()
    }

    /// When the next single start may be tried; `None` while healthy.
    pub fn retry_at(&self) -> Option<Millis> {
        self.retry_at
    }
}

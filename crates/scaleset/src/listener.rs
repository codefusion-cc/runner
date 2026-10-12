use std::future::Future;
use std::time::{Duration, Instant};

use tokio::sync::{Notify, watch};

use crate::error::Error;
use crate::session::MessageSession;
use crate::types::Message;

/// What turns the service's messages into Actions runners. [`listen`] calls it one message at a time, never twice at
/// once.
pub trait Scaler: Send {
    /// Handles the scale set's counts at the start (a message with only `statistics`), each batch of job messages,
    /// and `None` when the wait ended quietly. Handling a message again is harmless: after a failure the service
    /// sends it again. Returns how many Actions runners the machine would take now, which the next wait carries to the
    /// service. An error ends the session without acknowledging the message.
    fn scale(&mut self, message: Option<&Message>) -> impl Future<Output = Result<usize, Error>> + Send;

    /// Whether the session is to be opened again or the counts fetched, asked after every [`Scaler::scale`]:
    /// `Some((ZERO, look))` now, `Some((d, look))` in `d` unless the scaler is asked again before, `None` neither.
    /// The service looks at a scale set's queued jobs when a job is queued or ends and when a session opens, never
    /// because a wait carries more room than the one before: room that came while nothing happened at GitHub
    /// reaches its queued jobs only through a new session. What a look brings comes to [`Scaler::scale`] as a
    /// message with only `statistics`; a scaler that keeps saying `Some((ZERO, _))` is asked again after it.
    fn renew(&mut self) -> Option<(Duration, Look)> {
        None
    }
}

/// What a [`Scaler`] asks of the session besides its next message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// End the session and open a new one: the service looks at the scale set's queued jobs. What the old session's
    /// queue still held is lost.
    Reopen,
    /// [`Look::Reopen`], once a wait ended by itself and brought nothing: the queue was empty then, and the messages
    /// of a job that runs (its start, its end) are not lost with it.
    ReopenBetweenMessages,
    /// Fetch the scale set's counts, which otherwise come only with a message; the session and its queue stay.
    Recount,
}

/// Asks [`listen`] and [`supervise`] to stop.
#[derive(Debug)]
pub struct ShutdownTrigger(watch::Sender<bool>);

impl ShutdownTrigger {
    /// Asks to stop. A wait in progress ends at once; a message in the scaler's hands is finished first.
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }
}

/// Whether to stop.
#[derive(Debug, Clone)]
pub struct Shutdown(watch::Receiver<bool>);

/// A trigger and the signal it sets.
pub fn shutdown() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTrigger(tx), Shutdown(rx))
}

impl Shutdown {
    /// Whether stopping was asked for.
    pub fn is_set(&self) -> bool {
        *self.0.borrow()
    }

    /// Returns once stopping was asked for (at once when it was). When the trigger is dropped without having asked,
    /// nobody can ask any more and this never returns, so [`Shutdown::is_set`] and `wait` agree and a dropped
    /// trigger cannot make [`supervise`] open sessions in a loop.
    pub async fn wait(&mut self) {
        if self.0.wait_for(|set| *set).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// Polls the session and hands what arrives to the scaler, until `stop` is set (then `Ok`) or something fails.
///
/// Per message: the scaler handles it, then it is acknowledged, in that order, so a failure anywhere brings it
/// back. A message with an ID at or below one already handled in this session (delivered twice) is acknowledged
/// without being handled again. `stop` interrupts only the wait for a message: one received is handled and
/// acknowledged first, so nothing the scaler was given is dropped.
pub async fn listen(session: &MessageSession, scaler: &mut impl Scaler, stop: Shutdown) -> Result<(), Error> {
    listen_nudged(session, scaler, stop, &Notify::new()).await
}

/// [`listen`], with a wait for a message that a nudge ends too: the scaler is asked again at once, as after a
/// quiet poll, and the next wait carries the capacity it then gives. For what changes on the machine while nobody
/// writes: a slot that opened, a waiting job whose turn came. A nudge during the handling of a message is kept
/// for the next wait; several are one.
pub async fn listen_nudged(
    session: &MessageSession,
    scaler: &mut impl Scaler,
    mut stop: Shutdown,
    nudge: &Notify,
) -> Result<(), Error> {
    let mut capacity = scaler
        .scale(Some(&Message::initial(session.initial_statistics())))
        .await?;
    let mut handled = 0i64;
    // The last wait ran out by itself and brought nothing.
    let mut quiet = false;
    loop {
        if stop.is_set() {
            return Ok(());
        }
        let renew_at = match scaler.renew() {
            Some((after, look))
                if after.is_zero()
                    && (look == Look::Reopen || look == Look::ReopenBetweenMessages && quiet) =>
            {
                tracing::debug!(
                    scale_set = session.scale_set_id(),
                    "room GitHub has not looked at: the session is opened again"
                );
                let statistics = session.reopen().await?;
                handled = 0;
                capacity = scaler.scale(Some(&Message::initial(statistics))).await?;
                continue;
            }
            Some((after, Look::Recount)) if after.is_zero() => {
                let counted = session.recount().await?;
                capacity = scaler.scale(counted.map(Message::initial).as_ref()).await?;
                continue;
            }
            // Not before the queue is seen empty: the wait runs its course.
            Some((after, Look::ReopenBetweenMessages)) if after.is_zero() => None,
            Some((after, _)) => Some(tokio::time::Instant::now() + after),
            None => None,
        };
        let renewal = async {
            match renew_at {
                Some(at) => tokio::time::sleep_until(at).await,
                None => std::future::pending().await,
            }
        };
        let (polled, by_itself) = tokio::select! {
            biased;
            () = stop.wait() => return Ok(()),
            // The wait is given up; a message it was about to bring stays in the session's queue and comes with
            // the next one.
            () = nudge.notified() => (None, false),
            () = renewal => (None, false),
            polled = session.poll(handled, capacity) => (polled?, true),
        };
        quiet = by_itself && polled.is_none();
        match polled {
            None => capacity = scaler.scale(None).await?,
            Some(message) if message.id > 0 && message.id <= handled => {
                tracing::debug!(
                    id = message.id,
                    "a message arrived again; acknowledged without handling"
                );
                session.acknowledge(message.id).await?;
            }
            Some(message) => {
                capacity = scaler.scale(Some(&message)).await?;
                if message.id > 0 {
                    session.acknowledge(message.id).await?;
                    handled = message.id;
                }
            }
        }
    }
}

/// The longest `Retry-After` that is obeyed. GitHub's limits lift within the hour; an answer that asks for more
/// must not keep a scale set unserved for days.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

/// The longest wait while a session of this machine from before is still open. That is the state a Runner that
/// died leaves: GitHub drops the session by itself, and until a new one opens no job is taken and none that ended
/// is heard of, so the next try is never minutes away.
pub const SESSION_EXISTS_WAIT: Duration = Duration::from_secs(30);

/// How long to wait before opening a session again after one failed.
#[derive(Debug, Clone)]
pub struct Backoff {
    first: Duration,
    max: Duration,
    healthy_after: Duration,
    next: Duration,
}

impl Backoff {
    /// Starts at `first`, doubles up to `max`, and starts over after a session that lasted `healthy_after`.
    pub fn new(first: Duration, max: Duration, healthy_after: Duration) -> Backoff {
        Backoff {
            first,
            max,
            healthy_after,
            next: first,
        }
    }

    /// The Go manager's: 5 s up to 5 min, forgotten after 5 min of a working session.
    pub fn standard() -> Backoff {
        Backoff::new(
            Duration::from_secs(5),
            Duration::from_secs(300),
            Duration::from_secs(300),
        )
    }

    /// The wait after a session that ran `ran_for` and ended with `error`. Between half and the whole of the
    /// current step (the jitter keeps machines that lost the service together from coming back together), and never
    /// less than what a `Retry-After` asked for, up to [`MAX_RETRY_AFTER`]. `jitter` is in `0.0..=1.0`. While a
    /// session of before is still open the wait is [`SESSION_EXISTS_WAIT`] at most.
    pub fn next_wait(&mut self, ran_for: Duration, error: &Error, jitter: f64) -> Duration {
        if ran_for >= self.healthy_after {
            self.next = self.first;
        }
        let step = self.next;
        self.next = (self.next * 2).min(self.max);
        let wait = step.mul_f64(0.5 + 0.5 * jitter.clamp(0.0, 1.0));
        if matches!(error, Error::SessionExists) {
            return wait.min(SESSION_EXISTS_WAIT);
        }
        match error.retry_after() {
            Some(asked) => wait.max(asked.min(MAX_RETRY_AFTER)),
            None => wait,
        }
    }
}

/// Runs `serve` again and again, waiting by `backoff` between runs, until `stop` is set. `serve` is one session:
/// it ends with `Ok` only when asked to stop. `failed` hears of each failure and the wait that follows.
pub async fn supervise<F, Fut>(
    mut stop: Shutdown,
    mut backoff: Backoff,
    mut serve: F,
    mut failed: impl FnMut(&Error, Duration),
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), Error>>,
{
    while !stop.is_set() {
        let started = Instant::now();
        let error = match serve().await {
            Ok(()) => continue,
            Err(error) => error,
        };
        if stop.is_set() {
            return;
        }
        let wait = backoff.next_wait(started.elapsed(), &error, rand::random::<f64>());
        failed(&error, wait);
        tokio::select! {
            () = stop.wait() => return,
            () = tokio::time::sleep(wait) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_of_before_that_is_still_open_is_asked_for_again_within_half_a_minute() {
        let mut backoff = Backoff::standard();
        let waits: Vec<Duration> = (0..10)
            .map(|_| backoff.next_wait(Duration::ZERO, &Error::SessionExists, 1.0))
            .collect();
        assert_eq!(
            waits[..4],
            [5, 10, 20, 30].map(Duration::from_secs),
            "it starts as any failure does"
        );
        assert!(waits.iter().all(|wait| *wait <= SESSION_EXISTS_WAIT), "{waits:?}");
        // Any other failure after it waits as long as so many failures in a row do.
        let other = Error::Scaler("it would not answer".into());
        assert_eq!(
            backoff.next_wait(Duration::ZERO, &other, 1.0),
            Duration::from_secs(300)
        );
    }

    #[test]
    fn a_retry_after_of_days_is_not_obeyed_beyond_an_hour() {
        let asks = |secs| {
            Error::Api(cf_ghapp::Error::Status(cf_ghapp::StatusError {
                method: "GET".into(),
                path: "/".into(),
                status: 429,
                body: String::new(),
                retry_after: Some(Duration::from_secs(secs)),
                server_time: None,
            }))
        };
        let mut backoff = Backoff::new(
            Duration::from_secs(1),
            Duration::from_secs(8),
            Duration::from_secs(60),
        );
        assert_eq!(
            backoff.next_wait(Duration::ZERO, &asks(600), 1.0),
            Duration::from_secs(600)
        );
        assert_eq!(
            backoff.next_wait(Duration::ZERO, &asks(3600), 1.0),
            MAX_RETRY_AFTER
        );
        assert_eq!(
            backoff.next_wait(Duration::ZERO, &asks(7 * 24 * 3600), 1.0),
            MAX_RETRY_AFTER
        );
    }

    #[tokio::test]
    async fn a_dropped_trigger_does_not_end_the_wait() {
        let (trigger, mut stop) = shutdown();
        drop(trigger);
        let ended = tokio::time::timeout(Duration::from_millis(50), stop.wait()).await;
        assert!(ended.is_err(), "nobody asked to stop");
        assert!(!stop.is_set());
    }

    #[tokio::test]
    async fn a_trigger_ends_the_wait() {
        let (trigger, mut stop) = shutdown();
        trigger.trigger();
        tokio::time::timeout(Duration::from_secs(1), stop.wait())
            .await
            .unwrap();
        assert!(stop.is_set());
    }
}

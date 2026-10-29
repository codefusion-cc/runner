use std::time::Duration;

/// What the scale set service, or the way to it, can do to a call.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// GitHub, the network or a body went wrong; see [`cf_ghapp::Error`].
    #[error("{0}")]
    Api(#[from] cf_ghapp::Error),
    /// The service no longer knows the message session (or its scale set): a new session is needed.
    #[error("the message session is gone: {0}")]
    SessionLost(String),
    /// A session of this machine is still open on the service; it expires on its own shortly.
    #[error("a previous session of this machine is still open; it expires shortly")]
    SessionExists,
    /// The scaler failed on a message. The message was not acknowledged, so the service sends it again.
    #[error("the scaler failed: {0}")]
    Scaler(String),
}

impl Error {
    /// The HTTP status, when GitHub answered with one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Api(e) => e.status(),
            _ => None,
        }
    }

    /// What `Retry-After` asked for, when it did.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Api(e) => e.retry_after(),
            _ => None,
        }
    }

    /// Whether the console said no job waits for this Actions runner yet: a 409 that says when to ask again (a
    /// trusted host's `no_trusted_job`). Nothing was made, and no name is taken.
    pub fn is_no_job_yet(&self) -> bool {
        self.status() == Some(409) && self.retry_after().is_some()
    }

    /// Whether the thing asked about does not exist (404).
    pub fn is_not_found(&self) -> bool {
        self.status() == Some(404)
    }

    /// Whether waiting and calling again can work: network trouble, rate limits, server errors, a session that
    /// has to be opened again, or a session of this machine that has not expired yet. A refusal (401, 403) or a
    /// body that is not what the call needs does not mend by itself, but the worker still tries again, rarely.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Api(e) => e.is_transient(),
            Error::SessionLost(_) | Error::SessionExists | Error::Scaler(_) => true,
        }
    }
}

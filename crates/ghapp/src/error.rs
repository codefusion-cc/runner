use std::fmt;
use std::time::Duration;

/// A response GitHub answered with a status the call did not expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusError {
    /// `GET`, `POST` and so on.
    pub method: String,
    /// The request path (never the query: it may carry a cursor, never a secret, but is of no use here).
    pub path: String,
    /// The HTTP status.
    pub status: u16,
    /// The start of the body, at most 512 bytes, to say why.
    pub body: String,
    /// What the `Retry-After` header asked for, if it did.
    pub retry_after: Option<Duration>,
    /// GitHub's clock when it answered, from the `Date` header, in seconds since the epoch.
    pub server_time: Option<i64>,
}

impl StatusError {
    /// Whether GitHub refused for its rate limits, not for a permission: a 429, or a 403 that asks for a wait or
    /// says so. A primary limit's 403 asks for none.
    pub fn is_rate_limited(&self) -> bool {
        self.status == 429
            || (self.status == 403 && (self.retry_after.is_some() || self.body.contains("rate limit")))
    }
}

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}: {} {}", self.method, self.path, self.status, self.body)
    }
}

/// What a call to GitHub can meet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// GitHub answered with an unexpected status.
    #[error("{0}")]
    Status(StatusError),
    /// The request did not complete: no connection, a timeout, a body that ended early.
    #[error("network: {0}")]
    Transport(String),
    /// GitHub answered with a body that is not what the call needs.
    #[error("{what}: {why}")]
    Decode {
        /// The call.
        what: String,
        /// What was wrong.
        why: String,
    },
    /// The call was given up for safety: an address a token may not go to (not HTTPS, or a host the service has no
    /// business naming), or an answer too long to read.
    #[error("refused: {0}")]
    Refused(String),
}

impl Error {
    /// A body that is not the JSON a call needs: [`Error::Decode`] that says what kind of fault and where, and none
    /// of the body's text. serde's own message quotes the value it met, and an answer may hold a token.
    pub fn not_the_json(what: &str, error: &serde_json::Error) -> Error {
        use serde_json::error::Category;
        let kind = match error.classify() {
            Category::Io => "could not be read",
            Category::Syntax => "is not JSON",
            Category::Data => "is not what the call needs",
            Category::Eof => "ends early",
        };
        Error::Decode {
            what: what.into(),
            why: format!(
                "the answer {kind} (line {}, column {})",
                error.line(),
                error.column()
            ),
        }
    }

    /// The HTTP status, when GitHub answered with one.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status(s) => Some(s.status),
            _ => None,
        }
    }

    /// What `Retry-After` asked for, when it did.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Status(s) => s.retry_after,
            _ => None,
        }
    }

    /// Whether trying the same call again later can work: network trouble, rate limits and server errors do; a
    /// refusal (401, 403, 404, 409) and a malformed body do not by themselves.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Transport(_) => true,
            Error::Status(s) => {
                s.status == 429 || s.status >= 500 || (s.status == 403 && s.retry_after.is_some())
            }
            Error::Decode { .. } | Error::Refused(_) => false,
        }
    }
}

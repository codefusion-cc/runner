use std::time::Duration;

use cf_ghapp::Error as HttpError;

/// The code of the console's 403 while the owner has not turned GitHub through the console on for the machine, or
/// turned it off (`brokerOff` in its `worker/runners/github.ts`).
pub const BROKER_OFF: &str = "broker_off";
/// The longest `Retry-After` taken from the console, an hour (the most it asks for); a longer one is held to this.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(3600);

/// What a call to the console can meet. Nothing here carries a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The console does not know the token (or, on enrolment, the code), or the token is not one the console could
    /// know. The Mac enrols again.
    #[error("the console does not accept this token or code")]
    Unauthorized,
    /// The console refused the request with a 4xx other than 401: a message of its own, or one for the owner to read.
    /// Sending the same request again gets the same answer. A 403 that says [`BROKER_OFF`] is [`Error::GithubOff`].
    #[error("the console refused ({status}): {message}")]
    Refused {
        /// The HTTP status.
        status: u16,
        /// The console's `error` text, cleaned and cut to 300 characters.
        message: String,
    },
    /// GitHub through the console is off for this machine: the console answered 403 with the code [`BROKER_OFF`].
    /// The owner turns it on on the machine's page; until then the same call gets the same answer.
    #[error("the console refused (403): {0}")]
    GithubOff(String),
    /// The console could not be reached or failed: no connection, a timeout, a dropped connection, a 429 or a 5xx.
    /// Trying again later can work.
    #[error("the console cannot be reached: {0}")]
    Unavailable(String),
    /// The console answered a 429 or a 5xx that says how long to wait (`Retry-After`): asking again sooner gets the
    /// same answer.
    #[error("the console answered {status}: {message}")]
    Busy {
        /// The HTTP status.
        status: u16,
        /// The console's `error` text, cleaned and cut to 300 characters.
        message: String,
        /// What `Retry-After` asked for, at most [`RETRY_AFTER_MAX`].
        retry_after: Duration,
    },
    /// Something is not the contract's: a request this client would not send, or an answer that is not what protocol 1
    /// says (another status, a body that is not JSON of the right shape, an address that is not HTTPS).
    #[error("not the console's protocol: {0}")]
    Protocol(String),
}

impl Error {
    /// Whether the same call can work later with nothing changed: [`Error::Unavailable`] and [`Error::Busy`].
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Unavailable(_) | Error::Busy { .. })
    }

    /// How long the console asked to be left alone, when it said.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::Busy { retry_after, .. } => Some(*retry_after),
            _ => None,
        }
    }
}

/// The refusal's JSON body (at most the first 512 bytes), when it is JSON.
fn json_of(body: &str) -> Option<serde_json::Value> {
    serde_json::from_str::<serde_json::Value>(body).ok()
}

/// The console's `code` from a refusal's body: what tells one 403 from another. None when it names none.
fn code_of(body: &str) -> Option<String> {
    json_of(body)?.get("code")?.as_str().map(str::to_owned)
}

/// The console's `error` text from a refusal's body (at most the first 512 bytes), without control characters and
/// at most 300 characters. Not JSON, or JSON without it (an Access page, a proxy), is said so and not quoted.
pub(crate) fn message_of(body: &str) -> String {
    let text = json_of(body).and_then(|v| v.get("error")?.as_str().map(str::to_owned));
    match text {
        Some(text) => text.chars().filter(|c| !c.is_control()).take(300).collect(),
        None => "no message from the console".to_string(),
    }
}

impl From<HttpError> for Error {
    fn from(error: HttpError) -> Error {
        match error {
            HttpError::Status(s) => match s.status {
                401 => Error::Unauthorized,
                403 if code_of(&s.body).as_deref() == Some(BROKER_OFF) => {
                    Error::GithubOff(message_of(&s.body))
                }
                429 | 500..=599 => match s.retry_after {
                    Some(wait) => Error::Busy {
                        status: s.status,
                        message: message_of(&s.body),
                        retry_after: wait.min(RETRY_AFTER_MAX),
                    },
                    None => Error::Unavailable(format!(
                        "{} answered {}: {}",
                        s.path,
                        s.status,
                        message_of(&s.body)
                    )),
                },
                400..=499 => Error::Refused {
                    status: s.status,
                    message: message_of(&s.body),
                },
                other => Error::Protocol(format!("{} answered {other}", s.path)),
            },
            HttpError::Transport(why) => Error::Unavailable(why),
            HttpError::Refused(why) => Error::Protocol(why),
            HttpError::Decode { what, why } => Error::Protocol(format!("{what}: {why}")),
            HttpError::Key(_) => Error::Protocol("a key was asked for".into()),
        }
    }
}

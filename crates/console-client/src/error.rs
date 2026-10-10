use cf_ghapp::Error as HttpError;

/// What a call to the console can meet. Nothing here carries a token.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The console does not know the token (or, on enrolment, the code), or the token is not one the console could
    /// know. The Mac enrols again.
    #[error("the console does not accept this token or code")]
    Unauthorized,
    /// The console refused the request with a 4xx other than 401: a message of its own, or one for the owner to read.
    /// Sending the same request again gets the same answer.
    #[error("the console refused ({status}): {message}")]
    Refused {
        /// The HTTP status.
        status: u16,
        /// The console's `error` text, cleaned and cut to 300 characters.
        message: String,
    },
    /// The console could not be reached or failed: no connection, a timeout, a dropped connection, a 429 or a 5xx.
    /// Trying again later can work.
    #[error("the console cannot be reached: {0}")]
    Unavailable(String),
    /// Something is not the contract's: a request this client would not send, or an answer that is not what protocol 1
    /// says (another status, a body that is not JSON of the right shape, an address that is not HTTPS).
    #[error("not the console's protocol: {0}")]
    Protocol(String),
}

impl Error {
    /// Whether the same call can work later with nothing changed: only [`Error::Unavailable`].
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Unavailable(_))
    }
}

/// The console's `error` text from a refusal's body (at most the first 512 bytes), without control characters and
/// at most 300 characters. Not JSON, or JSON without it (an Access page, a proxy), is said so and not quoted.
pub(crate) fn message_of(body: &str) -> String {
    let text = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(str::to_owned));
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
                429 | 500..=599 => Error::Unavailable(format!(
                    "{} answered {}: {}",
                    s.path,
                    s.status,
                    message_of(&s.body)
                )),
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

use std::fmt;

use serde::{Deserialize, Serialize};

/// A value that must not reach a log: a one-job configuration of the Actions runner, a token. `Debug` prints no part
/// of it. It travels in a message as a plain string, since the other end needs it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself, for the one place that uses it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(hidden)")
    }
}

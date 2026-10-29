use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use cf_secret::Secret;

use crate::client::{Installation, PlainJit};
use crate::error::Error;

/// What a [`TokenSource`] call resolves to.
pub type Pending<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

/// Where a [`crate::GitHubApp`] gets its GitHub tokens: the CodeFusion Console, which holds the Runners App's key and
/// hands each machine tokens (`cf_console_client::ConsoleTokens`), or a test's stand-in.
///
/// Every call is a result: a source that cannot give a token says why, and nothing here panics on what the network
/// or the console does. A token is a [`Secret`] and appears in no error.
pub trait TokenSource: Send + Sync {
    /// The installations whose repositories [`TokenSource::token`] reads.
    fn installations(&self) -> Pending<'_, Vec<Installation>>;

    /// A token that reads the repositories of `installation`. `repo` names the repository a call is about, when it is
    /// about one: a source whose installations are not the ones the caller learned takes the installation that covers
    /// it instead.
    fn token<'a>(&'a self, installation: i64, repo: Option<&'a str>) -> Pending<'a, Secret<String>>;

    /// GitHub refused `token` (401): it is not handed out again, unless another call already replaced it.
    fn refused<'a>(
        &'a self,
        installation: i64,
        token: &'a Secret<String>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    /// The installation that covers `repo` (`owner/name`).
    fn repo_installation<'a>(&'a self, repo: &'a str) -> Pending<'a, i64>;

    /// Registers an Actions runner for one job with `repo`, outside any scale set, with exactly `labels` (see
    /// [`crate::GitHubApp::generate_jit_config`]). Sent once: a call that fails may still have registered it.
    fn generate_jit_config<'a>(
        &'a self,
        installation: i64,
        repo: &'a str,
        name: &'a str,
        labels: &'a [String],
    ) -> Pending<'a, PlainJit>;

    /// GitHub's time as this source knows it, in seconds since the epoch: what the expiry of a token GitHub issued is
    /// compared with.
    fn now_epoch(&self) -> i64;

    /// GitHub's clock minus this machine's, in seconds, when the source learned it; 0 when not.
    fn clock_skew_secs(&self) -> i64 {
        0
    }
}

/// A clock that never goes back, for how long a token given with a lifetime ("ends in 3600 s") lasts: the wall clock
/// may jump, this does not. A test moves it.
pub trait Monotonic: Send + Sync {
    /// The current instant.
    fn now(&self) -> Instant;
}

/// The machine's monotonic clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemMonotonic;

impl Monotonic for SystemMonotonic {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

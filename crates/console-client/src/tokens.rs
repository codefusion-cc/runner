//! [`ConsoleTokens`]: the GitHub tokens of a machine from the console, as a [`TokenSource`] of the App client.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, UNIX_EPOCH};

use cf_ghapp::{
    Clock, Error as GitHubError, Installation, Monotonic, Pending, PlainJit, StatusError, TokenSource,
};
use cf_secret::Secret;

use crate::client::Client;
use crate::error::Error;
use crate::github::is_runner_of;

/// A token is not used once less than this is left of it: a call made with it must end before it does.
pub const END_MARGIN: Duration = Duration::from_secs(60);
/// How long an answer that lists no installation stands before the console is asked again.
pub const EMPTY_LIFE: Duration = Duration::from_secs(300);
/// After the console said its tokens are off for this machine, how long until it is asked again, with a local key
/// to fall back on; [`GATE_RECHECK_ALONE`] without one.
pub const GATE_RECHECK: Duration = Duration::from_secs(300);
/// [`GATE_RECHECK`] for a machine that has nothing else: it is asked again sooner.
pub const GATE_RECHECK_ALONE: Duration = Duration::from_secs(60);
/// The first wait after a refresh failed; it doubles up to [`FAILED_WAIT_MAX`].
pub const FAILED_WAIT: Duration = Duration::from_secs(5);
/// The longest wait between refreshes that fail.
pub const FAILED_WAIT_MAX: Duration = Duration::from_secs(300);

const READ: &str = "/agent/runners/github/read";
const REGISTRATION: &str = "/agent/runners/github/registration";
const JIT: &str = "/agent/runners/github/jit";

/// One installation's token from the last read.
struct Held {
    id: i64,
    account: Option<String>,
    token: Secret<String>,
}

/// The console's last read answer, with when it is asked again and when its tokens stop being used, both on the
/// monotonic clock and counted from when the call was sent.
struct Read {
    held: Vec<Held>,
    refresh_at: Instant,
    ends_at: Instant,
}

impl Read {
    /// The installation an `installation` from a caller is: itself when it is one of these, else the one on the
    /// account that owns `repo` (a caller that learned its installations from the local key).
    fn find(&self, installation: i64, repo: Option<&str>) -> Option<&Held> {
        self.held
            .iter()
            .find(|h| h.id == installation)
            .or_else(|| self.owning(repo?))
    }

    /// The installation on the account that owns `repo` (`owner/name`), as GitHub compares names: without case.
    fn owning(&self, repo: &str) -> Option<&Held> {
        let owner = repo.split_once('/')?.0;
        self.held.iter().find(|h| {
            h.account
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(owner))
        })
    }
}

/// A refresh that failed, and when the console is asked again.
struct Failing {
    retry_at: Instant,
    failures: u32,
    error: GitHubError,
}

#[derive(Default)]
struct State {
    read: Option<Arc<Read>>,
    /// The installation IDs the console gave tokens for, kept after its tokens are dropped: an ID a caller learned
    /// from the console, met while the local key serves.
    console_ids: HashSet<i64>,
    /// A console installation ID and the local key's installation for the same repository, as last looked up.
    mapped: HashMap<i64, i64>,
    /// Until when the console is taken at its word that its tokens are off for this machine.
    off_until: Option<Instant>,
    failing: Option<Failing>,
    /// Tokens of the console GitHub refused (401). The console keeps a token while 40 minutes of it are left and hands
    /// it out again: one it does is not sent to GitHub, and the console is asked again only after a wait.
    refused: Vec<Secret<String>>,
}

/// The wait after `failures` refreshes in a row went wrong: [`FAILED_WAIT`], doubled each time, at most
/// [`FAILED_WAIT_MAX`].
fn failed_wait(failures: u32) -> Duration {
    FAILED_WAIT
        .saturating_mul(1u32 << failures.saturating_sub(1).min(16))
        .min(FAILED_WAIT_MAX)
}

/// Where a call goes.
enum Route<'a> {
    Console(Arc<Read>),
    Key(&'a dyn TokenSource),
}

/// The machine's GitHub tokens from the CodeFusion Console (`/agent/runners/github/*`), with the machine's own
/// console token. Read tokens are asked for once for all installations, kept until half their life has passed and
/// used until [`END_MARGIN`] before they end, by the monotonic clock; concurrent callers share one request.
///
/// While the console cannot be reached, a token it gave keeps being used until its end, and the console is asked
/// again after a wait that doubles from [`FAILED_WAIT`]; past the end every call is a clear error and none waits.
/// While the console answers 403 (the owner has not turned GitHub tokens on for this machine, or turned them off)
/// the local key, when there is one, serves everything, and the console is asked again after [`GATE_RECHECK`].
pub struct ConsoleTokens {
    client: Client,
    machine: String,
    fallback: Option<Arc<dyn TokenSource>>,
    clock: Arc<dyn Clock>,
    ticks: Arc<dyn Monotonic>,
    state: Mutex<State>,
    refreshing: tokio::sync::Mutex<()>,
}

impl std::fmt::Debug for ConsoleTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConsoleTokens")
            .field("console", &self.client.base())
            .field("machine", &self.machine)
            .field("fallback", &self.fallback.is_some())
            .finish_non_exhaustive()
    }
}

/// A console error as the App client's: what can work later stays transient, a refusal keeps its status, and none
/// carries a token.
fn as_github(path: &str, error: Error) -> GitHubError {
    let status = |status: u16, body: String| {
        GitHubError::Status(StatusError {
            method: "POST".into(),
            path: path.into(),
            status,
            body,
            retry_after: None,
            server_time: None,
        })
    };
    match error {
        Error::Unauthorized => status(
            401,
            "the console does not take this machine's token: enroll it again".into(),
        ),
        Error::Refused {
            status: code,
            message,
        } => status(code, message),
        Error::Unavailable(why) => GitHubError::Transport(format!("the console: {why}")),
        Error::Protocol(why) => GitHubError::Decode {
            what: format!("the console's answer to {path}"),
            why,
        },
    }
}

fn gate_off(error: &Error) -> bool {
    matches!(error, Error::Refused { status: 403, .. })
}

impl ConsoleTokens {
    /// The tokens `client` gets for the machine `machine` (its name at the console). `fallback` is the App's own key
    /// on this machine, when it has one: it serves while the console's tokens are off for the machine. `clock` is the
    /// wall clock (GitHub's time, for the scale set client), `ticks` the monotonic one the lifetimes are counted on.
    /// Nothing is asked of the console until a token is needed.
    pub fn new(
        client: Client,
        machine: &str,
        fallback: Option<Arc<dyn TokenSource>>,
        clock: Arc<dyn Clock>,
        ticks: Arc<dyn Monotonic>,
    ) -> ConsoleTokens {
        ConsoleTokens {
            client,
            machine: machine.to_string(),
            fallback,
            clock,
            ticks,
            state: Mutex::new(State::default()),
            refreshing: tokio::sync::Mutex::new(()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the console's tokens are off for this machine as far as it last said.
    fn is_off(&self, now: Instant) -> bool {
        self.lock().off_until.is_some_and(|until| now < until)
    }

    /// The console said 403: its tokens are dropped, and it is asked again after a while.
    fn mark_off(&self, why: &Error) {
        let wait = if self.fallback.is_some() {
            GATE_RECHECK
        } else {
            GATE_RECHECK_ALONE
        };
        let mut state = self.lock();
        let was_off = state.off_until.is_some();
        state.off_until = Some(self.ticks.now() + wait);
        state.read = None;
        state.failing = None;
        drop(state);
        if !was_off {
            match &self.fallback {
                Some(_) => {
                    tracing::info!(%why, "GitHub tokens through the console are off; the App key here serves")
                }
                None => {
                    tracing::warn!(%why, "GitHub tokens through the console are off, and there is no App key here")
                }
            }
        }
    }

    /// Where calls go while the console says its tokens are off: the local key, or an error that says so (`why`, the
    /// console's own refusal, when there is one).
    fn off(&self, why: Option<GitHubError>) -> Result<&dyn TokenSource, GitHubError> {
        match &self.fallback {
            Some(key) => Ok(key.as_ref()),
            None => Err(why.unwrap_or_else(|| {
                as_github(
                    READ,
                    Error::Refused {
                        status: 403,
                        message:
                            "GitHub tokens through the console are off for this machine, and it has no App \
                                  key (app_dir): the owner turns them on in the machine's page in the console"
                                .into(),
                    },
                )
            })),
        }
    }

    /// The error of a machine left with no token: the last refresh's, said so.
    fn none_left(error: &GitHubError) -> GitHubError {
        match error {
            GitHubError::Transport(why) => GitHubError::Transport(format!(
                "no GitHub token: the last one the console gave ended, and {why}"
            )),
            other => other.clone(),
        }
    }

    /// Asks the console for read tokens. Holds `refreshing` while it does (the caller took it).
    async fn refresh(&self) -> Result<Arc<Read>, Option<GitHubError>> {
        let sent = self.ticks.now();
        match self.client.github_read().await {
            Ok(tokens) => {
                let (refresh_at, ends_at) = match tokens.iter().map(|t| t.expires_in).min() {
                    Some(life) => (sent + life / 2, sent + life.saturating_sub(END_MARGIN)),
                    None => (sent + EMPTY_LIFE, sent + EMPTY_LIFE),
                };
                let mut read = Read {
                    held: tokens
                        .into_iter()
                        .map(|t| Held {
                            id: t.id,
                            account: t.account,
                            token: t.token,
                        })
                        .collect(),
                    refresh_at,
                    ends_at,
                };
                let mut state = self.lock();
                if state.off_until.take().is_some() {
                    tracing::info!("GitHub tokens come from the console again");
                }
                state
                    .refused
                    .retain(|token| read.held.iter().any(|h| h.token == *token));
                if state.refused.is_empty() {
                    if state.failing.take().is_some() {
                        tracing::info!("the console gives GitHub tokens again");
                    }
                } else {
                    // A token GitHub refused, handed out again: asked for again after a wait, not on every call.
                    let failures = state.failing.as_ref().map_or(0, |f| f.failures) + 1;
                    let retry_at = self.ticks.now() + failed_wait(failures);
                    read.refresh_at = read.refresh_at.min(retry_at);
                    let error = as_github(
                        READ,
                        Error::Refused {
                            status: 401,
                            message: "GitHub refuses a token the console gives again".into(),
                        },
                    );
                    tracing::warn!(%error, failures, "the console's token for an installation does not work at GitHub");
                    state.failing = Some(Failing {
                        retry_at,
                        failures,
                        error,
                    });
                }
                let read = Arc::new(read);
                state.console_ids.extend(read.held.iter().map(|h| h.id));
                state.read = Some(read.clone());
                Ok(read)
            }
            Err(error) if gate_off(&error) => {
                self.mark_off(&error);
                Err(self.fallback.is_none().then(|| as_github(READ, error)))
            }
            Err(error) => {
                let error = as_github(READ, error);
                let now = self.ticks.now();
                let mut state = self.lock();
                let failures = state.failing.as_ref().map_or(0, |f| f.failures) + 1;
                let wait = failed_wait(failures);
                // A token that still works is not left to end while nobody asks.
                let retry_at = match &state.read {
                    Some(read) if now < read.ends_at => (now + wait).min(read.ends_at),
                    _ => now + wait,
                };
                state.failing = Some(Failing {
                    retry_at,
                    failures,
                    error: error.clone(),
                });
                Err(Some(error))
            }
        }
    }

    /// Where a read goes now: the console's tokens (asked for when due, shared by every caller at the time), or the
    /// local key while the console's are off.
    async fn route(&self) -> Result<Route<'_>, GitHubError> {
        let now = self.ticks.now();
        if self.is_off(now) {
            return self.off(None).map(Route::Key);
        }
        let held = self.lock().read.clone();
        if let Some(read) = held.as_ref().filter(|r| now < r.refresh_at) {
            return Ok(Route::Console(read.clone()));
        }
        if let Some(read) = held.filter(|r| now < r.ends_at) {
            // Due, still good: one caller asks, the rest go on with it, and a console that fails changes nothing yet.
            let waiting = self.lock().failing.as_ref().is_some_and(|f| now < f.retry_at);
            if waiting {
                return Ok(Route::Console(read));
            }
            let Ok(_one) = self.refreshing.try_lock() else {
                return Ok(Route::Console(read));
            };
            return match self.refresh().await {
                Ok(fresh) => Ok(Route::Console(fresh)),
                Err(_) if self.is_off(self.ticks.now()) => self.off(None).map(Route::Key),
                Err(error) => {
                    let left = read.ends_at.saturating_duration_since(self.ticks.now()).as_secs();
                    if let Some(error) = error {
                        tracing::warn!(%error, left_s = left, "the console gave no new GitHub tokens; the last ones serve");
                    }
                    Ok(Route::Console(read))
                }
            };
        }
        // None to use: wait for the caller asking now, if any, and take what it got.
        let _one = self.refreshing.lock().await;
        let now = self.ticks.now();
        if self.is_off(now) {
            return self.off(None).map(Route::Key);
        }
        let (held, failing) = {
            let state = self.lock();
            let failing = state
                .failing
                .as_ref()
                .filter(|f| now < f.retry_at)
                .map(|f| f.error.clone());
            (state.read.clone(), failing)
        };
        if let Some(read) = held.filter(|r| now < r.ends_at) {
            return Ok(Route::Console(read));
        }
        if let Some(error) = failing {
            return Err(Self::none_left(&error));
        }
        match self.refresh().await {
            Ok(fresh) => Ok(Route::Console(fresh)),
            Err(why) if self.is_off(self.ticks.now()) => self.off(why).map(Route::Key),
            Err(Some(error)) => Err(Self::none_left(&error)),
            Err(None) => self.off(None).map(Route::Key),
        }
    }

    /// The local key's installation for `installation`: the same one, unless it is the console's and the call is
    /// about a repository, whose installation of the key's App is looked up.
    async fn key_installation(
        &self,
        key: &dyn TokenSource,
        installation: i64,
        repo: Option<&str>,
    ) -> Result<i64, GitHubError> {
        let (foreign, known) = {
            let state = self.lock();
            (
                state.console_ids.contains(&installation),
                state.mapped.get(&installation).copied(),
            )
        };
        match (foreign, repo) {
            (true, Some(repo)) => {
                if let Some(known) = known {
                    return Ok(known);
                }
                let found = key.repo_installation(repo).await?;
                self.lock().mapped.insert(installation, found);
                Ok(found)
            }
            _ => Ok(installation),
        }
    }

    async fn read_token(&self, installation: i64, repo: Option<&str>) -> Result<Secret<String>, GitHubError> {
        match self.route().await? {
            Route::Console(read) => match read.find(installation, repo) {
                Some(held) if self.lock().refused.contains(&held.token) => Err(as_github(
                    READ,
                    Error::Refused {
                        status: 401,
                        message: format!(
                            "GitHub refuses the token the console gives for installation {}; the console is asked \
                             again after a wait",
                            held.id
                        ),
                    },
                )),
                Some(held) => Ok(held.token.clone()),
                None => Err(as_github(
                    READ,
                    Error::Refused {
                        status: 404,
                        message: format!(
                            "the console gives no token for installation {installation}{}",
                            repo.map(|r| format!(" or {r}")).unwrap_or_default()
                        ),
                    },
                )),
            },
            Route::Key(key) => {
                let installation = self.key_installation(key, installation, repo).await?;
                key.token(installation, repo).await
            }
        }
    }

    async fn forget(&self, installation: i64, token: &Secret<String>) {
        {
            let mut state = self.lock();
            let ours = state
                .read
                .as_ref()
                .is_some_and(|read| read.held.iter().any(|h| h.token == *token));
            if ours {
                // Asked for again at once, the first time: the console hands out the same token while it keeps it.
                if !state.refused.contains(token) {
                    state.refused.push(token.clone());
                    state.read = None;
                }
                return;
            }
        }
        if let Some(key) = &self.fallback {
            let mapped = self.lock().mapped.get(&installation).copied();
            key.refused(mapped.unwrap_or(installation), token).await;
        }
    }

    async fn installations_now(&self) -> Result<Vec<Installation>, GitHubError> {
        match self.route().await? {
            Route::Console(read) => Ok(read.held.iter().map(|h| Installation { id: h.id }).collect()),
            Route::Key(key) => key.installations().await,
        }
    }

    async fn installation_of(&self, repo: &str) -> Result<i64, GitHubError> {
        match self.route().await? {
            Route::Console(read) => read.owning(repo).map(|h| h.id).ok_or_else(|| {
                as_github(
                    READ,
                    Error::Refused {
                        status: 404,
                        message: format!("the console's Runners App is not installed on the owner of {repo}"),
                    },
                )
            }),
            Route::Key(key) => key.repo_installation(repo).await,
        }
    }

    async fn registration(&self, installation: i64, repo: &str) -> Result<Secret<String>, GitHubError> {
        let mut why = None;
        if !self.is_off(self.ticks.now()) {
            match self.client.github_registration(repo).await {
                Ok(token) => return Ok(token.token),
                Err(error) if gate_off(&error) => {
                    self.mark_off(&error);
                    why = Some(as_github(REGISTRATION, error));
                }
                Err(error) => return Err(as_github(REGISTRATION, error)),
            }
        }
        let key = self.off(why)?;
        let installation = self.key_installation(key, installation, Some(repo)).await?;
        key.registration_token(installation, repo).await
    }

    async fn jit(
        &self,
        installation: i64,
        repo: &str,
        name: &str,
        labels: &[String],
    ) -> Result<PlainJit, GitHubError> {
        let mut why = None;
        if !self.is_off(self.ticks.now()) {
            if !is_runner_of(&self.machine, name) {
                return Err(GitHubError::Refused(format!(
                    "{name}: the console makes only Actions runners named {}-j and seven hex digits",
                    self.machine
                )));
            }
            // The console refuses a machine whose switch is off before it asks GitHub anything, so the key may try.
            match self.client.github_jit(repo, name, labels).await {
                Ok(jit) => {
                    return Ok(PlainJit {
                        id: jit.runner_id,
                        encoded: jit.encoded,
                    });
                }
                Err(error) if gate_off(&error) => {
                    self.mark_off(&error);
                    why = Some(as_github(JIT, error));
                }
                Err(error) => return Err(as_github(JIT, error)),
            }
        }
        let key = self.off(why)?;
        let installation = self.key_installation(key, installation, Some(repo)).await?;
        key.generate_jit_config(installation, repo, name, labels).await
    }
}

impl TokenSource for ConsoleTokens {
    fn installations(&self) -> Pending<'_, Vec<Installation>> {
        Box::pin(self.installations_now())
    }

    fn token<'a>(&'a self, installation: i64, repo: Option<&'a str>) -> Pending<'a, Secret<String>> {
        Box::pin(self.read_token(installation, repo))
    }

    fn refused<'a>(
        &'a self,
        installation: i64,
        token: &'a Secret<String>,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(self.forget(installation, token))
    }

    fn repo_installation<'a>(&'a self, repo: &'a str) -> Pending<'a, i64> {
        Box::pin(self.installation_of(repo))
    }

    fn registration_token<'a>(&'a self, installation: i64, repo: &'a str) -> Pending<'a, Secret<String>> {
        Box::pin(self.registration(installation, repo))
    }

    fn generate_jit_config<'a>(
        &'a self,
        installation: i64,
        repo: &'a str,
        name: &'a str,
        labels: &'a [String],
    ) -> Pending<'a, PlainJit> {
        Box::pin(self.jit(installation, repo, name, labels))
    }

    fn now_epoch(&self) -> i64 {
        match &self.fallback {
            Some(key) => key.now_epoch(),
            None => self
                .clock
                .now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs() as i64,
        }
    }

    fn clock_skew_secs(&self) -> i64 {
        self.fallback.as_ref().map_or(0, |key| key.clock_skew_secs())
    }
}

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_console_client::{
    BAD_ADDRESS, Client as Console, Error as ConsoleError, NAME_TAKEN, OpenedSession, RefreshedSession,
    Removal, SESSION_EXISTS, SESSION_LOST,
};
use cf_ghapp::{Error as ApiError, GitHubApp, KeyTokens, Response};
use cf_secret::Secret;
use reqwest::header::AUTHORIZATION;
use reqwest::{Method, Url};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::Error;
use crate::key::{KeyAdmin, Registrar};
use crate::session::MessageSession;
use crate::types::{JitConfig, Label, RunnerRef, ScaleSet};

pub(crate) const API_VERSION: &str = "6.0-preview";

/// How long a console that answered it has no scale set calls (a 404 without a code: older than this Runner) is
/// taken at its word: until then this host makes those calls itself, with the console's registration token, and asks
/// the console again after it.
pub const OLDER_CONSOLE_RECHECK: Duration = Duration::from_secs(600);

/// Where github.com's Actions service lives: the addresses it hands back are on this host or one below it.
pub const ACTIONS_HOST: &str = "actions.githubusercontent.com";

/// One repository's scale sets on the Actions service, reached through the CodeFusion Console.
///
/// The console holds the Runners App's key, the registration tokens and the service's admin token: every
/// administrator's call (scale sets, sessions, Actions runners) is the console's `github/scaleset/*`, made for this
/// machine and scoped to it. The machine holds only a session's message queue token, sent to GitHub's addresses
/// only, and each Actions runner's configuration. A host that still holds the App's key ([`ScaleSetClient::with_key`])
/// makes these calls itself until the console's answered it, and so does a host whose console is older than these
/// calls, with the console's registration token: a Runner never needs a console newer than the one that runs. Every call returns a `Result`; nothing here panics for
/// what the console, the service or the network does.
#[derive(Clone)]
pub struct ScaleSetClient {
    inner: Arc<Inner>,
}

/// Who makes the administrator's calls.
enum Admin {
    /// The console, for this machine; this host while the console has no such calls.
    Console(Console, OlderConsole),
    /// This host, with the App's key it still holds.
    Key(KeyAdmin),
}

/// This host's own calls, with the console's registration token, for a console older than its scale set calls; and
/// until when the console is not asked them, since it last answered that it has none.
struct OlderConsole {
    host: KeyAdmin,
    until: Mutex<Option<tokio::time::Instant>>,
}

struct Inner {
    app: GitHubApp,
    admin: Admin,
    machine: String,
    installation: i64,
    repo: String,
    poll_timeout: Duration,
}

impl std::fmt::Debug for ScaleSetClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScaleSetClient")
            .field("repo", &self.inner.repo)
            .field("machine", &self.inner.machine)
            .finish_non_exhaustive()
    }
}

pub(crate) fn decode<T: DeserializeOwned>(what: &str, response: &Response) -> Result<T, Error> {
    serde_json::from_slice(&response.body).map_err(|e| Error::Api(ApiError::not_the_json(what, &e)))
}

/// Why no token may go to `address`, which the service handed back; `None` when one may. It must be an HTTPS address
/// (or this machine itself, as in tests), and either at the API's own origin or on a host of the Actions service
/// ([`ACTIONS_HOST`] or below). An answer cannot send a token anywhere else.
fn address_refusal(api: &str, address: &str) -> Option<String> {
    let Ok(url) = Url::parse(address) else {
        return Some("the service named no usable address".into());
    };
    let host = url.host_str().unwrap_or("");
    if !cf_ghapp::is_safe_address(&url) {
        return Some(format!("the service named {host}, which is not an HTTPS address"));
    }
    let on_the_api = Url::parse(api).is_ok_and(|api| cf_ghapp::same_origin(&api, &url));
    let on_the_service = host == ACTIONS_HOST
        || host
            .strip_suffix(ACTIONS_HOST)
            .is_some_and(|below| below.ends_with('.'));
    if on_the_api || on_the_service {
        None
    } else {
        Some(format!("the service named {host}, which is not GitHub's"))
    }
}

/// The console's refusal of a scale set call (`route`) as this crate's error. Its codes keep their meaning: a session
/// GitHub lost is [`Error::SessionLost`], one still open [`Error::SessionExists`], a queue that is not GitHub's a
/// refusal for safety. A 404 without a code is a console without the route, never a thing that is gone. Everything
/// else keeps its status and its wait (409 `name_taken`, 422 `runner_busy`, 429, 503).
fn from_console(route: &str, error: ConsoleError) -> Error {
    let path = format!("/agent/runners/github/scaleset/{route}");
    match (&error, error.code()) {
        (
            ConsoleError::Refused {
                status: 404, message, ..
            },
            Some(SESSION_LOST),
        ) => Error::SessionLost(message.clone()),
        (ConsoleError::Refused { status: 409, .. }, Some(SESSION_EXISTS)) => Error::SessionExists,
        (ConsoleError::Refused { message, .. }, Some(BAD_ADDRESS)) => {
            Error::Api(ApiError::Refused(format!("{route}: {message}")))
        }
        (
            ConsoleError::Refused {
                status: 404, message, ..
            },
            None,
        ) => Error::Api(ApiError::Refused(format!(
            "{route}: the console has no such route ({message}): it is older than this Runner"
        ))),
        _ => Error::Api(error.to_github(&path)),
    }
}

fn set_of(entry: cf_console_client::ScaleSetEntry) -> ScaleSet {
    ScaleSet {
        id: entry.id,
        name: entry.name,
        labels: entry
            .labels
            .into_iter()
            .map(|name| Label {
                kind: "System".into(),
                name,
            })
            .collect(),
    }
}

impl ScaleSetClient {
    /// A client for the scale sets of `repo` (`owner/name`) through `console`, for the machine named `machine` there.
    /// `app` reads GitHub (an Actions runner's state) and registers Actions runners outside scale sets, through
    /// `installation`. `poll_timeout` is how long a long poll may take in all; the service ends it after about a
    /// minute.
    pub fn new(
        app: GitHubApp,
        console: Console,
        machine: &str,
        installation: i64,
        repo: &str,
        config_url: &str,
        poll_timeout: Duration,
    ) -> ScaleSetClient {
        let older = OlderConsole {
            host: KeyAdmin::new(Registrar::Console(console.clone()), config_url),
            until: Mutex::new(None),
        };
        ScaleSetClient::of(
            app,
            Admin::Console(console, older),
            machine,
            installation,
            repo,
            poll_timeout,
        )
    }

    /// A client for the scale sets of `repo` (`owner/name`, whose page is `config_url`) of a host that still holds the
    /// Runners App's key, `tokens`: it buys the Actions service's admin token itself and makes every administrator's
    /// call with it, as the machine `machine`. Only until the console's scale set calls answered the host, which then
    /// deletes the key.
    pub fn with_key(
        app: GitHubApp,
        tokens: Arc<KeyTokens>,
        machine: &str,
        installation: i64,
        repo: &str,
        config_url: &str,
        poll_timeout: Duration,
    ) -> ScaleSetClient {
        let admin = Admin::Key(KeyAdmin::new(Registrar::Key(tokens), config_url));
        ScaleSetClient::of(app, admin, machine, installation, repo, poll_timeout)
    }

    fn of(
        app: GitHubApp,
        admin: Admin,
        machine: &str,
        installation: i64,
        repo: &str,
        poll_timeout: Duration,
    ) -> ScaleSetClient {
        ScaleSetClient {
            inner: Arc::new(Inner {
                app,
                admin,
                machine: machine.to_string(),
                installation,
                repo: repo.to_string(),
                poll_timeout,
            }),
        }
    }

    /// Whether this client makes the administrator's calls with the App's key on this host, not through the console.
    pub fn holds_key(&self) -> bool {
        matches!(self.inner.admin, Admin::Key(_))
    }

    /// Who makes the administrator's calls now: the console, or this host (with the key it holds, or with the
    /// console's registration token while the console is taken to have no scale set calls).
    fn admin(&self) -> Result<&Console, &KeyAdmin> {
        match &self.inner.admin {
            Admin::Key(key) => Err(key),
            Admin::Console(console, older) => {
                let until = *older.until.lock().unwrap_or_else(|e| e.into_inner());
                match until {
                    Some(until) if tokio::time::Instant::now() < until => Err(&older.host),
                    _ => Ok(console),
                }
            }
        }
    }

    /// This host's calls when `error` is the console's 404 without a code, which says it is older than its scale set
    /// calls: they are made here from now on, for [`OLDER_CONSOLE_RECHECK`].
    fn older_console(&self, route: &str, error: &ConsoleError) -> Option<&KeyAdmin> {
        let Admin::Console(_, older) = &self.inner.admin else {
            return None;
        };
        if !matches!(error, ConsoleError::Refused { status: 404, .. }) || error.code().is_some() {
            return None;
        }
        let mut until = older.until.lock().unwrap_or_else(|e| e.into_inner());
        if until.is_none() {
            tracing::warn!(repo = %self.inner.repo, route, "the console has no scale set calls, it is older than this Runner: this host makes them with the console's registration token");
        }
        *until = Some(tokio::time::Instant::now() + OLDER_CONSOLE_RECHECK);
        Some(&older.host)
    }

    pub(crate) fn app(&self) -> &GitHubApp {
        &self.inner.app
    }

    pub(crate) fn installation(&self) -> i64 {
        self.inner.installation
    }

    pub(crate) fn repo(&self) -> &str {
        &self.inner.repo
    }

    /// Refuses an address the service handed back that [`address_refusal`] names a reason against.
    pub(crate) fn check_address(&self, what: &str, address: &str) -> Result<(), Error> {
        match address_refusal(self.inner.app.api(), address) {
            None => Ok(()),
            Some(why) => Err(Error::Api(ApiError::Refused(format!("{what}: {why}")))),
        }
    }

    pub(crate) fn poll_timeout(&self) -> Duration {
        self.inner.poll_timeout
    }

    /// One call with a session's token to GitHub's queue or service.
    pub(crate) async fn send(
        &self,
        method: Method,
        url: &str,
        token: &Secret<String>,
        body: Option<&Value>,
        timeout: Option<Duration>,
        max_capacity: Option<usize>,
    ) -> Result<Response, Error> {
        // Every request with a session's token passes here, so whatever address a caller was handed, the token goes
        // to GitHub only.
        self.check_address("request", url)?;
        let bearer = format!("Bearer {}", token.expose());
        let payload = body.map(Value::to_string);
        Ok(self
            .inner
            .app
            .http()
            .send(|c| {
                let mut r = c
                    .request(method.clone(), url)
                    .header(AUTHORIZATION, &bearer)
                    .header("Accept", "application/json");
                if let Some(n) = max_capacity {
                    r = r.header("X-ScaleSetMaxCapacity", n.to_string());
                }
                if let Some(p) = &payload {
                    r = r.header("Content-Type", "application/json").body(p.clone());
                }
                if let Some(t) = timeout {
                    r = r.timeout(t);
                }
                r
            })
            .await?)
    }

    /// The machine's scale sets of the repository (those named `<machine>--…` in the Default runner group).
    pub async fn list_scale_sets(&self) -> Result<Vec<ScaleSet>, Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => return self.host_list(host).await,
        };
        match console.scaleset_list(&self.inner.repo).await {
            Ok(sets) => Ok(sets.into_iter().map(set_of).collect()),
            Err(e) => match self.older_console("list", &e) {
                Some(host) => self.host_list(host).await,
                None => Err(from_console("list", e)),
            },
        }
    }

    /// The machine's scale sets as this host lists them.
    async fn host_list(&self, host: &KeyAdmin) -> Result<Vec<ScaleSet>, Error> {
        let mine = |set: &ScaleSet| cf_console_client::is_set_name_of(&self.inner.machine, &set.name);
        Ok(host.list(self).await?.into_iter().filter(mine).collect())
    }

    /// The machine's scale set of this name, if there is one.
    pub async fn find_scale_set(&self, name: &str) -> Result<Option<ScaleSet>, Error> {
        Ok(self.list_scale_sets().await?.into_iter().find(|s| s.name == name))
    }

    /// Finds the scale set of this name with exactly these labels, creating it or setting its labels when needed.
    pub async fn ensure_scale_set(&self, name: &str, labels: &[String]) -> Result<ScaleSet, Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => return host.ensure(self, name, labels).await,
        };
        match console
            .scaleset_create(&self.inner.machine, &self.inner.repo, name, labels)
            .await
        {
            Ok(entry) => Ok(set_of(entry)),
            Err(e) => match self.older_console("create", &e) {
                Some(host) => host.ensure(self, name, labels).await,
                None => Err(from_console("create", e)),
            },
        }
    }

    /// Deletes a scale set; its queued jobs wait for another Actions runner. Whether it was there to delete.
    pub async fn delete_scale_set(&self, id: i64) -> Result<bool, Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => return host.delete(self, id).await,
        };
        match console.scaleset_delete(&self.inner.repo, id).await {
            Err(e) => match self.older_console("delete", &e) {
                Some(host) => host.delete(self, id).await,
                None => Err(from_console("delete", e)),
            },
            Ok(done) => Ok(done),
        }
    }

    /// Opens the message session of a scale set, owned by this machine. A session that is still open makes this
    /// [`Error::SessionExists`]: it expires, and the call can be made again.
    pub async fn open_session(&self, scale_set_id: i64) -> Result<MessageSession, Error> {
        let machine = &self.inner.machine;
        let opened: OpenedSession = match self.admin() {
            Ok(console) => match console
                .scaleset_session_open(&self.inner.repo, scale_set_id)
                .await
            {
                Ok(opened) => opened,
                Err(e) => match self.older_console("session/open", &e) {
                    Some(host) => host.open_session(self, scale_set_id, machine).await?,
                    None => return Err(from_console("session/open", e)),
                },
            },
            Err(host) => host.open_session(self, scale_set_id, machine).await?,
        };
        let checked = self
            .check_address("message session", &opened.queue_url)
            .and_then(|()| self.check_address("Actions service", &opened.service_url));
        if let Err(refused) = checked {
            // The service holds a session nobody will use: the next one would be refused until it ran out.
            if let Err(e) = self.delete_session(scale_set_id, &opened.session_id).await {
                tracing::warn!(error = %e, "could not end a session whose address was refused");
            }
            return Err(refused);
        }
        Ok(MessageSession::new(self.clone(), scale_set_id, opened))
    }

    /// Asks for a new token for a session. The addresses that come with it are checked as an opening's are.
    pub(crate) async fn refresh_session(
        &self,
        scale_set_id: i64,
        session_id: &str,
    ) -> Result<RefreshedSession, Error> {
        let refreshed = match self.admin() {
            Ok(console) => match console
                .scaleset_session_refresh(&self.inner.repo, scale_set_id, session_id)
                .await
            {
                Ok(refreshed) => refreshed,
                Err(e) => match self.older_console("session/refresh", &e) {
                    Some(host) => host.refresh_session(self, scale_set_id, session_id).await?,
                    None => return Err(from_console("session/refresh", e)),
                },
            },
            Err(host) => host.refresh_session(self, scale_set_id, session_id).await?,
        };
        // A refresh may leave the queue's address out; the session then keeps the one it has.
        if let Some(queue) = &refreshed.queue_url {
            self.check_address("message session", queue)?;
        }
        self.check_address("Actions service", &refreshed.service_url)?;
        Ok(refreshed)
    }

    /// Ends a session.
    pub(crate) async fn delete_session(&self, scale_set_id: i64, session_id: &str) -> Result<(), Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => return host.delete_session(self, scale_set_id, session_id).await,
        };
        match console
            .scaleset_session_close(&self.inner.repo, scale_set_id, session_id)
            .await
        {
            Ok(_) => Ok(()),
            Err(e) => match self.older_console("session/close", &e) {
                Some(host) => host.delete_session(self, scale_set_id, session_id).await,
                None => Err(from_console("session/close", e)),
            },
        }
    }

    /// Registers an Actions runner of the scale set that takes one job and unregisters itself after it. A name that
    /// is taken is a conflict (409 in the error): names are random, so another try uses another name. The console
    /// sends it to GitHub once.
    ///
    /// GitHub may register the Actions runner although the answer never arrives. Nobody holds the configuration of
    /// such a registration, so it is removed here before the error comes back. Give this only names made for this
    /// call.
    pub async fn generate_jit_config(&self, scale_set_id: i64, name: &str) -> Result<JitConfig, Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => {
                let answer = host.generate_jit(self, scale_set_id, name).await;
                return self.registered(name, answer).await;
            }
        };
        let answer = console
            .scaleset_jit(&self.inner.machine, &self.inner.repo, scale_set_id, name)
            .await
            .map(|jit| {
                let runner = RunnerRef {
                    id: jit.runner_id,
                    name: name.to_string(),
                };
                (Some(runner), jit.encoded)
            })
            .map_err(|e| {
                // The console answers every refusal of GitHub's with a 409; only `name_taken` says one is there. Its
                // 503 (GitHub busy or gone mid-call) and a dropped answer may follow a registration GitHub made; its
                // 429 is its own limit, which refused the call before GitHub was asked, and its 409 that says when to
                // ask again (no job for a trusted machine yet) made nothing either.
                let own_limit = matches!(
                    e,
                    ConsoleError::Busy {
                        status: 429 | 409,
                        ..
                    }
                );
                let leftover = e.code() == Some(NAME_TAKEN) || (e.is_transient() && !own_limit);
                (e, leftover)
            });
        let answer = match answer {
            Ok(answer) => Ok(answer),
            Err((e, leftover)) => match self.older_console("jit", &e) {
                Some(host) => host.generate_jit(self, scale_set_id, name).await,
                None => Err((from_console("jit", e), leftover)),
            },
        };
        self.registered(name, answer).await
    }

    /// Registers an Actions runner that takes one job with the repository, outside any scale set, carrying exactly
    /// `labels` (`docs/github-protocol.md`, probe M): GitHub may hand it a queued job whose labels it has, and looks
    /// at its queue when it comes online (probe N). Its registration is removed as a scale set's is
    /// ([`ScaleSetClient::remove_runner`]). A failure that may have left a registration behind removes it, as
    /// [`ScaleSetClient::generate_jit_config`] does. Give this only names made for this call.
    pub async fn generate_plain_jit_config(&self, name: &str, labels: &[String]) -> Result<JitConfig, Error> {
        let answer = self
            .inner
            .app
            .generate_jit_config(self.inner.installation, &self.inner.repo, name, labels)
            .await
            .map(|jit| {
                let runner = RunnerRef {
                    id: jit.id,
                    name: name.to_string(),
                };
                (Some(runner), jit.encoded)
            })
            .map_err(|e| {
                // A 429 is a limit that refused the call before anything was made (GitHub's, or the console's own, or
                // a wait the console asked for that kept it unsent), and so is a 409 that says when to ask again (the
                // console has no job for a trusted machine yet): there is nothing to look for.
                let e = Error::from(e);
                let leftover = !e.is_no_job_yet()
                    && (e.status() == Some(409) || (e.is_transient() && e.status() != Some(429)));
                (e, leftover)
            });
        self.registered(name, answer).await
    }

    /// The configuration of a registration the console answered, or the error with whether the call may have
    /// registered one anyway. One that may have been registered
    /// although the answer never arrived or was refused as taken is removed before the error comes back, and so is
    /// one whose ID is unknown, which could never be removed again, or that has no configuration and never starts.
    async fn registered(
        &self,
        name: &str,
        answer: Result<(Option<RunnerRef>, Secret<String>), (Error, bool)>,
    ) -> Result<JitConfig, Error> {
        let (runner, encoded) = match answer {
            Ok(answer) => answer,
            Err((e, leftover)) => {
                if leftover {
                    self.remove_leftover(name).await;
                }
                return Err(e);
            }
        };
        let why = match runner.filter(|runner| runner.id > 0) {
            Some(runner) if !encoded.expose().is_empty() => return Ok(JitConfig { runner, encoded }),
            Some(_) => "the service sent no configuration",
            None => "the service did not say which Actions runner it registered",
        };
        self.remove_leftover(name).await;
        Err(Error::Api(ApiError::Decode {
            what: "Actions runner configuration".into(),
            why: why.into(),
        }))
    }

    /// Removes the registration a failed [`ScaleSetClient::generate_jit_config`] may have left, as far as the
    /// service lets it: one that took a job in the meantime stays.
    async fn remove_leftover(&self, name: &str) {
        match self.remove_runner_named(name).await {
            Ok(true) => {
                tracing::info!(runner = %name, "removed a registration whose configuration never arrived")
            }
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(runner = %name, error = %e, "could not remove a registration without a machine")
            }
        }
    }

    /// Whether a registered Actions runner of the repository has a job, by GitHub's own record; `None` when GitHub no
    /// longer has it, as after a one-job Actions runner's job.
    pub async fn runner_busy(&self, runner_id: i64) -> Result<Option<bool>, Error> {
        Ok(self
            .inner
            .app
            .runner_busy(self.inner.installation, &self.inner.repo, runner_id)
            .await?)
    }

    /// Removes a registered Actions runner of this machine by GitHub's ID: whether it was there to remove. One with a
    /// job is refused (status 422); one of another machine too (403).
    pub async fn remove_runner(&self, runner_id: i64) -> Result<bool, Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => return host.remove_runner(self, runner_id).await,
        };
        match console
            .scaleset_remove(&self.inner.machine, &self.inner.repo, Removal::Id(runner_id))
            .await
        {
            Err(e) => match self.older_console("remove", &e) {
                Some(host) => host.remove_runner(self, runner_id).await,
                None => Err(from_console("remove", e)),
            },
            Ok(done) => Ok(done),
        }
    }

    /// Removes a registered Actions runner of this machine by name, as [`ScaleSetClient::remove_runner`] does.
    pub async fn remove_runner_named(&self, name: &str) -> Result<bool, Error> {
        let console = match self.admin() {
            Ok(console) => console,
            Err(host) => return host.remove_runner_named(self, name).await,
        };
        match console
            .scaleset_remove(&self.inner.machine, &self.inner.repo, Removal::Name(name))
            .await
        {
            Err(e) => match self.older_console("remove", &e) {
                Some(host) => host.remove_runner_named(self, name).await,
                None => Err(from_console("remove", e)),
            },
            Ok(done) => Ok(done),
        }
    }
}

/// Percent-encodes a value for a query or a path segment.
pub(crate) fn encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_goes_only_to_the_api_and_the_actions_service() {
        let api = "https://api.github.com";
        for (address, allowed) in [
            (
                "https://pipelinesghubeus2.actions.githubusercontent.com/abc/",
                true,
            ),
            ("https://broker.actions.githubusercontent.com/message", true),
            ("https://actions.githubusercontent.com/x", true),
            ("https://ACTIONS.githubusercontent.com/x", true),
            ("https://api.github.com/somewhere", true),
            ("http://pipelines.actions.githubusercontent.com/abc", false),
            ("https://raw.githubusercontent.com/x", false),
            ("https://evilactions.githubusercontent.com/x", false),
            ("https://actions.githubusercontent.com.example.org/x", false),
            ("https://example.org/actions.githubusercontent.com", false),
            ("https://user@example.org/x", false),
            ("https://actions.githubusercontent.com@example.org/x", false),
            ("https://api.github.com:8443/x", false),
            ("https://github.com/x", false),
            ("https://127.0.0.1/x", false),
            ("http://127.0.0.1:9/x", false),
            ("file:///etc/passwd", false),
            ("//actions.githubusercontent.com/x", false),
            ("", false),
        ] {
            assert_eq!(address_refusal(api, address).is_none(), allowed, "{address}");
        }
        // A fake on this machine is its own service, at its own port only.
        assert!(address_refusal("http://127.0.0.1:4000", "http://127.0.0.1:4000/svc").is_none());
        assert!(address_refusal("http://127.0.0.1:4000", "http://127.0.0.1:4001/svc").is_some());
    }
}

//! `github/scaleset/<route>` (the console's scale set routes, F1): the console holds the Actions service's admin token
//! and asks the service for the machine, which gets only what is its own: its scale sets, a session's message queue
//! token and address, the service's address, an Actions runner's configuration.
//!
//! The checks run in the console's order: the machine's token (401), the body (415, 413, 400), the machine's switch
//! (403 `broker_off`), the schema (400), the machine's rate (429 `rate_limited`), the scope (403 `not_yours`), the
//! Runners App (409), then GitHub. The admin token comes from the attached [`FakeGithub`] in-process, as the console
//! makes it with a registration token of its own: neither `/registration-token` nor `/actions/runner-registration`
//! is ever a request. Every service call is a real HTTP request to the fake, made with no lock held, marked with
//! [`CONSOLE_MARK`] and bounded by [`GITHUB_DEADLINE`].
//!
//! One difference from the console: GitHub's hosts are `actions.githubusercontent.com` and below over HTTPS, and here
//! also the fake GitHub's own origin (plain HTTP on the loopback interface), as the fake serves the service there.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cf_fake_github::httpd::{Reply, Request};
use cf_fake_github::{CONSOLE_MARK, FakeGithub};
use serde_json::{Value, json};

use crate::{GithubCall, State, coded, error, json_body, schema};

/// The fixed window of the rate limits, in seconds.
pub const RATE_WINDOW_S: i64 = 5 * 60;
/// Calls a machine may make in a window to `list`, `create` and `delete`.
pub const SETS_PER_WINDOW: u32 = 120;
/// Calls a machine may make in a window to `session/open`, `session/refresh` and `session/close`.
pub const SESSIONS_PER_WINDOW: u32 = 120;
/// Calls a machine may make in a window to `jit`.
pub const JIT_RUNNERS_PER_WINDOW: u32 = 300;
/// Calls a machine may make in a window to `remove`.
pub const REMOVALS_PER_WINDOW: u32 = 600;
/// A kept admin token is used while more than this is left of it, in seconds.
pub const ADMIN_REUSE_S: i64 = 120;
/// Every call to GitHub ends by then.
pub const GITHUB_DEADLINE: Duration = Duration::from_secs(15);

/// The path every scale set route starts with.
pub(crate) const PREFIX: &str = "/agent/runners/github/scaleset/";

/// One row of the console's audit of scale set calls (`runner_scaleset_calls`), as the contract has it.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    /// `set-create`, `set-delete`, `session-open`, `session-refresh`, `session-close`, `jit` or `remove`.
    pub kind: &'static str,
    /// The machine, by name.
    pub machine: String,
    /// The repository, lowercase.
    pub repo: String,
    pub set_id: Option<i64>,
    pub runner: Option<String>,
    /// `ok`, `refused` or `failed`.
    pub outcome: &'static str,
    /// At most 300 characters, never a token.
    pub detail: String,
}

/// `runner_scale_sets`: which machine a scale set is of, written only from GitHub's own answers.
pub(crate) struct Owned {
    repo: String,
    set_id: i64,
    machine: u64,
    name: String,
}

/// `runner_scaleset_admins`: the admin token kept for a repository.
#[derive(Clone)]
pub(crate) struct Admin {
    url: String,
    token: String,
    expires_at: i64,
}

#[derive(Default)]
pub(crate) struct Scalesets {
    owned: Vec<Owned>,
    admins: Vec<(String, Admin)>,
    rates: Vec<((u64, &'static str, i64), u32)>,
    audit: Vec<AuditRow>,
    /// The `Retry-After` of a `github_busy` when GitHub named none; `None` for the console's 30 seconds.
    busy_wait: Option<u32>,
}

impl Scalesets {
    /// A machine left: its ownership rows go with it (agent.ts forgetStatements).
    pub(crate) fn forget(&mut self, machine: u64) {
        self.owned.retain(|o| o.machine != machine);
    }

    pub(crate) fn forget_admins(&mut self) {
        self.admins.clear();
    }

    pub(crate) fn audit(&self) -> Vec<AuditRow> {
        self.audit.clone()
    }

    pub(crate) fn set_busy_wait(&mut self, seconds: u32) {
        self.busy_wait = Some(seconds);
    }
}

fn bucket(route: &str) -> (&'static str, u32) {
    match route {
        "list" | "create" | "delete" => ("sets", SETS_PER_WINDOW),
        "jit" => ("jit", JIT_RUNNERS_PER_WINDOW),
        "remove" => ("remove", REMOVALS_PER_WINDOW),
        _ => ("sessions", SESSIONS_PER_WINDOW),
    }
}

/// A scale set call that passed every check before GitHub, to be made with no lock held.
pub(crate) struct Call {
    route: String,
    machine: u64,
    name: String,
    body: Value,
    github: FakeGithub,
}

/// What the first, locked half of a scale set call came to.
pub(crate) enum Step {
    Done(Reply),
    Call(Box<Call>),
}

fn not_yours(what: &str) -> Reply {
    coded(403, &format!("{what} is not this machine's"), "not_yours")
}

/// The `Retry-After` of a `github_busy` when GitHub named none.
const BUSY_WAIT_S: u32 = 30;

fn busy(retry_after: Option<&str>, default: u32) -> Reply {
    let wait = retry_after
        .and_then(|v| v.trim().parse::<i64>().ok())
        .map_or(i64::from(default), |s| s.clamp(1, 3600));
    coded(503, "GitHub is busy: ask again later", "github_busy").with_header("retry-after", &wait.to_string())
}

fn bad_gateway(message: &str) -> Reply {
    error(502, message)
}

fn status_of(reply: &Reply) -> u16 {
    match reply {
        Reply::Response { status, .. } => *status,
        _ => 0,
    }
}

impl State {
    /// The locked half: every check before GitHub, in the console's order.
    pub(crate) fn scaleset_begin(&mut self, route: &str, request: &Request) -> Step {
        let machine = match self.machine_of(request) {
            Ok(machine) => machine,
            Err(reply) => return Step::Done(reply),
        };
        let body = match json_body(request) {
            Ok(body) => body,
            Err(reply) => return Step::Done(reply),
        };
        let name = self.name_of(machine);
        let reply = match self.override_next.take() {
            Some(reply) => Some(reply),
            None => self.scaleset_checks(route, machine, &name, &body),
        };
        if let Some(reply) = reply {
            self.record(route, &name, &body, &reply);
            return Step::Done(reply);
        }
        let Some(github) = self.github.clone() else {
            let reply = error(
                409,
                "The console holds no Runners App: an owner makes it on the GitHub Apps page",
            );
            self.record(route, &name, &body, &reply);
            return Step::Done(reply);
        };
        Step::Call(Box::new(Call {
            route: route.to_string(),
            machine,
            name,
            body,
            github,
        }))
    }

    fn record(&mut self, route: &str, name: &str, body: &Value, reply: &Reply) {
        self.github_calls.push(GithubCall {
            route: format!("scaleset/{route}"),
            machine: name.to_string(),
            body: body.clone(),
            status: status_of(reply),
        });
    }

    /// The switch, the schema, the rate and the scope; none when the call goes on.
    fn scaleset_checks(&mut self, route: &str, machine: u64, name: &str, body: &Value) -> Option<Reply> {
        if !self.machines.iter().any(|m| m.id == machine && m.broker) {
            return Some(self.broker_is_off(name));
        }
        match schema::scaleset(route, body) {
            None => return Some(error(404, "Not found")),
            Some(Err(problem)) => {
                return Some(error(
                    400,
                    &format!("Not a request this console reads: {problem}"),
                ));
            }
            Some(Ok(())) => {}
        }
        // Counted for every call that passed the schema, refused ones too.
        let now = self.github.as_ref().map_or(0, FakeGithub::now_epoch);
        let window = now.div_euclid(RATE_WINDOW_S);
        let (bucket, limit) = bucket(route);
        let rates = &mut self.scalesets.rates;
        rates.retain(|((_, _, w), _)| *w >= window - 1);
        let count = match rates.iter_mut().find(|(k, _)| *k == (machine, bucket, window)) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                rates.push(((machine, bucket, window), 1));
                1
            }
        };
        if count > limit {
            let wait = ((window + 1) * RATE_WINDOW_S - now).max(1);
            return Some(
                coded(
                    429,
                    &format!("Too many calls in five minutes: ask again in {wait} s"),
                    "rate_limited",
                )
                .with_header("retry-after", &wait.to_string()),
            );
        }
        let repo = body["repo"].as_str().unwrap_or("").to_lowercase();
        if route == "create" {
            let set = body["name"].as_str().unwrap_or("");
            if schema::set_machine(set) != Some(name) {
                return Some(not_yours(&format!("The scale set {set}")));
            }
        }
        if let Some(set_id) = body["setId"].as_i64() {
            let mine = self.scalesets.owned.iter().any(|o| {
                o.repo == repo
                    && o.set_id == set_id
                    && o.machine == machine
                    && o.name.starts_with(&format!("{name}--"))
            });
            if !mine {
                self.audit_row(
                    route,
                    name,
                    &repo,
                    Some(set_id),
                    None,
                    "refused",
                    "not this machine's",
                );
                return Some(not_yours(&format!("The scale set {set_id}")));
            }
        }
        if let Some(runner) = body["name"]
            .as_str()
            .filter(|_| route == "jit" || route == "remove")
            && schema::runner_machine(runner) != Some(name)
        {
            self.audit_row(
                route,
                name,
                &repo,
                None,
                Some(runner),
                "refused",
                "not this machine's",
            );
            return Some(not_yours(&format!("The runner {runner}")));
        }
        None
    }

    pub(crate) fn broker_is_off(&self, name: &str) -> Reply {
        coded(
            403,
            &format!(
                "GitHub through the console is off for {name}: the owner turns it on on the machine's page in the console"
            ),
            "broker_off",
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn audit_row(
        &mut self,
        route: &str,
        machine: &str,
        repo: &str,
        set_id: Option<i64>,
        runner: Option<&str>,
        outcome: &'static str,
        detail: &str,
    ) {
        let kind = match route {
            "create" => "set-create",
            "delete" => "set-delete",
            "session/open" => "session-open",
            "session/refresh" => "session-refresh",
            "session/close" => "session-close",
            "jit" => "jit",
            "remove" => "remove",
            _ => return,
        };
        self.scalesets.audit.push(AuditRow {
            kind,
            machine: machine.to_string(),
            repo: repo.to_string(),
            set_id,
            runner: runner.map(String::from),
            outcome,
            detail: detail.chars().take(300).collect(),
        });
    }

    /// requireStill: the machine is still there with its switch on, else the reply that says it is not.
    fn still(&self, machine: u64, name: &str) -> Option<Reply> {
        match self.machines.iter().find(|m| m.id == machine) {
            None => Some(error(401, "Unknown token")),
            Some(m) if !m.broker => Some(self.broker_is_off(name)),
            Some(_) => None,
        }
    }
}

/// A service answer: status, JSON (`null` for none), `Retry-After`.
struct Answer {
    status: u16,
    json: Value,
}

/// The unlocked half of a call: GitHub, with the admin token, then the answer. Records the call when done.
pub(crate) async fn run(state: &Arc<Mutex<State>>, call: Call) -> Reply {
    let ctx = Ctx {
        state: state.clone(),
        http: reqwest::Client::builder()
            .timeout(GITHUB_DEADLINE)
            .build()
            .expect("an HTTP client"),
        repo: call.body["repo"].as_str().unwrap_or("").to_string(),
        key: call.body["repo"].as_str().unwrap_or("").to_lowercase(),
        github: call.github.clone(),
        machine: call.machine,
        name: call.name.clone(),
        busy_wait: state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .scalesets
            .busy_wait
            .unwrap_or(BUSY_WAIT_S),
    };
    let reply = match ctx.route(&call.route, &call.body).await {
        Ok(reply) | Err(reply) => reply,
    };
    ctx.lock().record(&call.route, &call.name, &call.body, &reply);
    reply
}

struct Ctx {
    state: Arc<Mutex<State>>,
    http: reqwest::Client,
    github: FakeGithub,
    /// As the machine sent it.
    repo: String,
    /// Lowercase.
    key: String,
    machine: u64,
    name: String,
    /// The `Retry-After` of a `github_busy` when GitHub named none.
    busy_wait: u32,
}

/// Whether `url` is GitHub's: HTTPS on `actions.githubusercontent.com` or below, or the fake GitHub's own origin.
fn on_github(url: &str, fake: &str) -> bool {
    if let Some(rest) = url.strip_prefix(fake) {
        return rest.is_empty() || rest.starts_with('/');
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.to_ascii_lowercase();
    !host.contains('@')
        && !host.contains(':')
        && (host == "actions.githubusercontent.com" || host.ends_with(".actions.githubusercontent.com"))
}

fn message_of(json: &Value) -> String {
    json["message"]
        .as_str()
        .unwrap_or("GitHub refused the call")
        .chars()
        .take(300)
        .collect()
}

impl Ctx {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn audit(
        &self,
        route: &str,
        set_id: Option<i64>,
        runner: Option<&str>,
        outcome: &'static str,
        detail: &str,
    ) {
        self.lock()
            .audit_row(route, &self.name, &self.key, set_id, runner, outcome, detail);
    }

    /// The kept admin token while more than [`ADMIN_REUSE_S`] of it is left, else a new one, kept.
    fn admin(&self) -> Result<Admin, Reply> {
        let now = self.github.now_epoch();
        let mut st = self.lock();
        let admins = &mut st.scalesets.admins;
        admins.retain(|(_, a)| a.expires_at > now);
        if let Some((_, a)) = admins
            .iter()
            .find(|(repo, a)| *repo == self.key && a.expires_at - ADMIN_REUSE_S > now)
        {
            return Ok(a.clone());
        }
        let Some((url, token, expires_at)) = self.github.mint_admin_token(&self.repo) else {
            return Err(error(409, "Not Found"));
        };
        if !on_github(&url, &self.github.url()) {
            return Err(bad_gateway("GitHub named a service that is not GitHub's"));
        }
        let admin = Admin {
            url,
            token,
            expires_at,
        };
        admins.retain(|(repo, _)| *repo != self.key);
        admins.push((self.key.clone(), admin.clone()));
        Ok(admin)
    }

    /// Forgets the kept admin token if it is still `refused`.
    fn refused(&self, refused: &str) {
        self.lock()
            .scalesets
            .admins
            .retain(|(repo, a)| !(*repo == self.key && a.token == refused));
    }

    /// One service call with the admin token; a 401 replaces the token once and the call is made again once. GitHub's
    /// 429 and 5xx, a dropped connection and a timeout are a 503 `github_busy`.
    async fn service(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(Answer, String), Reply> {
        let mut retried = false;
        loop {
            let admin = self.admin()?;
            let joiner = if path.contains('?') { '&' } else { '?' };
            let url = format!("{}/_apis/{path}{joiner}api-version=6.0-preview", admin.url);
            let answer = self
                .send(
                    self.http.request(method.clone(), &url).bearer_auth(&admin.token),
                    body.as_ref(),
                )
                .await?;
            if answer.status == 401 && !retried {
                retried = true;
                self.refused(&admin.token);
                continue;
            }
            return Ok((answer, admin.url));
        }
    }

    async fn send(&self, request: reqwest::RequestBuilder, body: Option<&Value>) -> Result<Answer, Reply> {
        let mut request = request.header(CONSOLE_MARK, "1");
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .body(body.to_string());
        }
        let Ok(response) = request.send().await else {
            return Err(busy(None, self.busy_wait));
        };
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .map(String::from);
        if status == 429 || status >= 500 {
            return Err(busy(retry_after.as_deref(), self.busy_wait));
        }
        let Ok(bytes) = response.bytes().await else {
            return Err(busy(None, self.busy_wait));
        };
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Answer { status, json })
    }

    async fn route(&self, route: &str, body: &Value) -> Result<Reply, Reply> {
        let set_id = body["setId"].as_i64().unwrap_or(0);
        match route {
            "list" => self.list().await,
            "create" => self.create(body).await,
            "delete" => self.delete(set_id).await,
            "session/open" => self.open(set_id).await,
            "session/refresh" => {
                self.refresh(set_id, body["sessionId"].as_str().unwrap_or(""))
                    .await
            }
            "session/close" => self.close(set_id, body["sessionId"].as_str().unwrap_or("")).await,
            "jit" => self.jit(set_id, body["name"].as_str().unwrap_or("")).await,
            _ => self.remove(body).await,
        }
    }

    /// A scale set as the machine gets it, and its ownership row when it is the machine's.
    fn take(&self, set: &Value) -> Option<Value> {
        let id = set["id"].as_i64().filter(|id| *id > 0)?;
        let name = set["name"].as_str()?;
        let labels: Vec<String> = set["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l["name"].as_str().map(str::to_lowercase))
            .collect();
        if !name.starts_with(&format!("{}--", self.name)) {
            return None;
        }
        let mut st = self.lock();
        let owned = &mut st.scalesets.owned;
        owned.retain(|o| !(o.repo == self.key && o.set_id == id));
        owned.push(Owned {
            repo: self.key.clone(),
            set_id: id,
            machine: self.machine,
            name: name.to_string(),
        });
        Some(json!({ "id": id, "name": name, "labels": labels }))
    }

    async fn list(&self) -> Result<Reply, Reply> {
        let (answer, _) = self
            .service(
                reqwest::Method::GET,
                "runtime/runnerscalesets?runnerGroupId=1",
                None,
            )
            .await?;
        if answer.status != 200 {
            return Err(error(409, &message_of(&answer.json)));
        }
        let sets: Vec<Value> = answer.json["value"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s["runnerGroupId"].as_i64() == Some(1))
            .filter_map(|s| self.take(s))
            .collect();
        Ok(Reply::json(&json!({ "scaleSets": sets })))
    }

    async fn create(&self, body: &Value) -> Result<Reply, Reply> {
        let name = body["name"].as_str().unwrap_or("");
        let mut labels: Vec<String> = body["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|l| l.as_str().map(str::to_lowercase))
            .collect();
        labels.sort();
        labels.dedup();
        let (found, _) = self
            .service(
                reqwest::Method::GET,
                &format!("runtime/runnerscalesets?runnerGroupId=1&name={name}"),
                None,
            )
            .await?;
        if found.status != 200 {
            return Err(error(409, &message_of(&found.json)));
        }
        let existing = found.json["value"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| s["name"].as_str() == Some(name))
            .cloned();
        let wanted = json!({
            "name": name, "runnerGroupId": 1,
            "labels": labels.iter().map(|l| json!({ "type": "System", "name": l })).collect::<Vec<_>>(),
            "RunnerSetting": { "disableUpdate": true }, "enabled": true,
        });
        let (set, detail) = match existing {
            None => {
                let (made, _) = self
                    .service(reqwest::Method::POST, "runtime/runnerscalesets", Some(wanted))
                    .await?;
                if made.status != 200 && made.status != 201 {
                    self.audit("create", None, None, "failed", &message_of(&made.json));
                    return Err(error(409, &message_of(&made.json)));
                }
                (made.json, Some("created"))
            }
            Some(set) => {
                let mut has: Vec<String> = set["labels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["name"].as_str().map(str::to_lowercase))
                    .collect();
                has.sort();
                has.dedup();
                if has == labels {
                    (set, None)
                } else {
                    let id = set["id"].as_i64().unwrap_or(0);
                    let (changed, _) = self
                        .service(
                            reqwest::Method::PATCH,
                            &format!("runtime/runnerscalesets/{id}"),
                            Some(wanted),
                        )
                        .await?;
                    if changed.status != 200 {
                        self.audit("create", Some(id), None, "failed", &message_of(&changed.json));
                        return Err(error(409, &message_of(&changed.json)));
                    }
                    (changed.json, Some("labels set"))
                }
            }
        };
        let Some(entry) = self.take(&set) else {
            return Err(bad_gateway("GitHub's answer has no scale set"));
        };
        if let Some(detail) = detail {
            self.audit("create", entry["id"].as_i64(), None, "ok", detail);
        }
        Ok(Reply::json(&json!({ "scaleSet": entry })))
    }

    fn forget_set(&self, set_id: i64) {
        self.lock()
            .scalesets
            .owned
            .retain(|o| !(o.repo == self.key && o.set_id == set_id));
    }

    async fn delete(&self, set_id: i64) -> Result<Reply, Reply> {
        let (answer, _) = self
            .service(
                reqwest::Method::DELETE,
                &format!("runtime/runnerscalesets/{set_id}"),
                None,
            )
            .await?;
        let deleted = match answer.status {
            200..=299 => true,
            404 => false,
            _ => {
                self.audit("delete", Some(set_id), None, "failed", &message_of(&answer.json));
                return Err(error(409, &message_of(&answer.json)));
            }
        };
        self.forget_set(set_id);
        self.audit(
            "delete",
            Some(set_id),
            None,
            "ok",
            if deleted { "deleted" } else { "gone" },
        );
        Ok(Reply::json(&json!({ "deleted": deleted })))
    }

    async fn drop_session(&self, set_id: i64, session: &str) {
        let _ = self
            .service(
                reqwest::Method::DELETE,
                &format!("runtime/runnerscalesets/{set_id}/sessions/{session}"),
                None,
            )
            .await;
    }

    async fn open(&self, set_id: i64) -> Result<Reply, Reply> {
        let (answer, service_url) = self
            .service(
                reqwest::Method::POST,
                &format!("runtime/runnerscalesets/{set_id}/sessions"),
                Some(json!({ "ownerName": self.name })),
            )
            .await?;
        match answer.status {
            200 | 201 => {}
            409 => {
                self.audit("session/open", Some(set_id), None, "refused", "a session exists");
                return Err(coded(409, &message_of(&answer.json), "session_exists"));
            }
            _ => {
                self.audit(
                    "session/open",
                    Some(set_id),
                    None,
                    "failed",
                    &message_of(&answer.json),
                );
                return Err(error(409, &message_of(&answer.json)));
            }
        }
        let json = &answer.json;
        let (Some(session), Some(queue), Some(token)) = (
            json["sessionId"].as_str().filter(|s| !s.is_empty()),
            json["messageQueueUrl"].as_str(),
            json["messageQueueAccessToken"].as_str().filter(|s| !s.is_empty()),
        ) else {
            return Err(bad_gateway("GitHub's session has no id, queue or token"));
        };
        if !on_github(queue, &self.github.url()) {
            self.drop_session(set_id, session).await;
            self.audit(
                "session/open",
                Some(set_id),
                None,
                "failed",
                "the queue is not GitHub's",
            );
            return Err(coded(
                502,
                "GitHub named a message queue that is not GitHub's",
                "bad_address",
            ));
        }
        let still = self.lock().still(self.machine, &self.name);
        if let Some(reply) = still {
            self.drop_session(set_id, session).await;
            return Err(reply);
        }
        let stat = |key: &str| json["statistics"][key].as_i64().unwrap_or(0);
        self.audit("session/open", Some(set_id), None, "ok", session);
        Ok(Reply::json(&json!({
            "sessionId": session,
            "messageQueueUrl": queue,
            "messageQueueAccessToken": token,
            "serviceUrl": service_url,
            "statistics": {
                "totalAvailableJobs": stat("totalAvailableJobs"),
                "totalAcquiredJobs": stat("totalAcquiredJobs"),
                "totalAssignedJobs": stat("totalAssignedJobs"),
                "totalRunningJobs": stat("totalRunningJobs"),
                "totalRegisteredRunners": stat("totalRegisteredRunners"),
                "totalBusyRunners": stat("totalBusyRunners"),
                "totalIdleRunners": stat("totalIdleRunners"),
            },
        })))
    }

    async fn refresh(&self, set_id: i64, session: &str) -> Result<Reply, Reply> {
        let (answer, service_url) = self
            .service(
                reqwest::Method::PATCH,
                &format!("runtime/runnerscalesets/{set_id}/sessions/{session}"),
                None,
            )
            .await?;
        match answer.status {
            200..=299 => {}
            404 => {
                self.audit("session/refresh", Some(set_id), None, "refused", "session lost");
                return Err(coded(404, &message_of(&answer.json), "session_lost"));
            }
            _ => {
                self.audit(
                    "session/refresh",
                    Some(set_id),
                    None,
                    "failed",
                    &message_of(&answer.json),
                );
                return Err(error(409, &message_of(&answer.json)));
            }
        }
        let Some(token) = answer.json["messageQueueAccessToken"]
            .as_str()
            .filter(|s| !s.is_empty())
        else {
            return Err(bad_gateway("GitHub's session has no token"));
        };
        let queue = answer.json["messageQueueUrl"].as_str().unwrap_or("");
        if !queue.is_empty() && !on_github(queue, &self.github.url()) {
            self.audit(
                "session/refresh",
                Some(set_id),
                None,
                "failed",
                "the queue is not GitHub's",
            );
            return Err(coded(
                502,
                "GitHub named a message queue that is not GitHub's",
                "bad_address",
            ));
        }
        self.audit("session/refresh", Some(set_id), None, "ok", session);
        Ok(Reply::json(&json!({
            "messageQueueUrl": queue, "messageQueueAccessToken": token, "serviceUrl": service_url,
        })))
    }

    async fn close(&self, set_id: i64, session: &str) -> Result<Reply, Reply> {
        let (answer, _) = self
            .service(
                reqwest::Method::DELETE,
                &format!("runtime/runnerscalesets/{set_id}/sessions/{session}"),
                None,
            )
            .await?;
        let closed = match answer.status {
            200..=299 => true,
            404 => false,
            _ => {
                self.audit(
                    "session/close",
                    Some(set_id),
                    None,
                    "failed",
                    &message_of(&answer.json),
                );
                return Err(error(409, &message_of(&answer.json)));
            }
        };
        self.audit("session/close", Some(set_id), None, "ok", session);
        Ok(Reply::json(&json!({ "closed": closed })))
    }

    async fn delete_agent(&self, id: i64) -> Result<Answer, Reply> {
        Ok(self
            .service(
                reqwest::Method::DELETE,
                &format!("distributedtask/pools/0/agents/{id}"),
                None,
            )
            .await?
            .0)
    }

    async fn jit(&self, set_id: i64, runner: &str) -> Result<Reply, Reply> {
        // The attempt is a row before GitHub is asked.
        self.audit("jit", Some(set_id), Some(runner), "ok", "asked");
        let answer = self
            .service(
                reqwest::Method::POST,
                &format!("runtime/runnerscalesets/{set_id}/generatejitconfig"),
                Some(json!({ "name": runner, "workFolder": "_work" })),
            )
            .await;
        let answer = match answer {
            Ok((answer, _)) => answer,
            Err(reply) => {
                self.audit("jit", Some(set_id), Some(runner), "failed", "GitHub is busy");
                return Err(reply);
            }
        };
        match answer.status {
            200 | 201 => {}
            409 => {
                self.audit("jit", Some(set_id), Some(runner), "refused", "name taken");
                return Err(coded(409, &message_of(&answer.json), "name_taken"));
            }
            _ => {
                self.audit(
                    "jit",
                    Some(set_id),
                    Some(runner),
                    "failed",
                    &message_of(&answer.json),
                );
                return Err(error(409, &message_of(&answer.json)));
            }
        }
        let id = answer.json["runner"]["id"].as_i64().filter(|id| *id > 0);
        let config = answer.json["encodedJITConfig"].as_str().filter(|c| !c.is_empty());
        let (Some(id), Some(config)) = (id, config) else {
            return Err(bad_gateway("GitHub's answer has no runner or no configuration"));
        };
        let still = self.lock().still(self.machine, &self.name);
        if let Some(reply) = still {
            let _ = self.delete_agent(id).await;
            return Err(reply);
        }
        Ok(Reply::json(
            &json!({ "runnerId": id, "encodedJitConfig": config }),
        ))
    }

    async fn remove(&self, body: &Value) -> Result<Reply, Reply> {
        let (id, runner) = if let Some(name) = body["name"].as_str() {
            let (found, _) = self
                .service(
                    reqwest::Method::GET,
                    &format!("distributedtask/pools/0/agents?agentName={name}"),
                    None,
                )
                .await?;
            if found.status != 200 {
                return Err(error(409, &message_of(&found.json)));
            }
            let id = found.json["value"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|a| a["name"].as_str() == Some(name))
                .and_then(|a| a["id"].as_i64());
            match id {
                Some(id) => (id, name.to_string()),
                None => {
                    self.audit("remove", None, Some(name), "ok", "not registered");
                    return Ok(Reply::json(&json!({ "removed": false })));
                }
            }
        } else {
            let id = body["runnerId"].as_i64().unwrap_or(0);
            // The REST runner's name, read with the console's installation token.
            let (token, _) = self.github.mint_installation_token();
            let url = format!("{}/repos/{}/actions/runners/{id}", self.github.url(), self.repo);
            let read = self.send(self.http.get(&url).bearer_auth(&token), None).await?;
            match read.status {
                200 => {}
                404 => {
                    self.audit("remove", None, None, "ok", "not registered");
                    return Ok(Reply::json(&json!({ "removed": false })));
                }
                _ => return Err(error(409, &message_of(&read.json))),
            }
            let name = read.json["name"].as_str().unwrap_or("").to_string();
            if schema::runner_machine(&name) != Some(self.name.as_str()) {
                self.audit("remove", None, Some(&name), "refused", "not this machine's");
                return Err(not_yours(&format!("The runner {id}")));
            }
            (id, name)
        };
        let answer = self.delete_agent(id).await?;
        let removed = match answer.status {
            200..=299 => true,
            404 => false,
            422 => {
                self.audit("remove", None, Some(&runner), "refused", "busy");
                return Err(coded(422, &message_of(&answer.json), "runner_busy"));
            }
            _ => {
                self.audit("remove", None, Some(&runner), "failed", &message_of(&answer.json));
                return Err(error(409, &message_of(&answer.json)));
            }
        };
        self.audit(
            "remove",
            None,
            Some(&runner),
            "ok",
            if removed { "removed" } else { "gone" },
        );
        Ok(Reply::json(&json!({ "removed": removed })))
    }
}

#[cfg(test)]
mod tests {
    use super::on_github;

    #[test]
    fn github_hosts_only() {
        let fake = "http://127.0.0.1:4000";
        assert!(on_github("https://actions.githubusercontent.com/x", fake));
        assert!(on_github(
            "https://pipelines.actions.githubusercontent.com/x",
            fake
        ));
        assert!(on_github("http://127.0.0.1:4000/svc", fake));
        assert!(!on_github("http://127.0.0.1:40001/svc", fake));
        assert!(!on_github("http://actions.githubusercontent.com/x", fake));
        assert!(!on_github("https://evil-actions.githubusercontent.com/x", fake));
        assert!(!on_github(
            "https://actions.githubusercontent.com.evil.example/x",
            fake
        ));
        assert!(!on_github(
            "https://actions.githubusercontent.com@evil.example/x",
            fake
        ));
        assert!(!on_github("https://actions.githubusercontent.com:8443/x", fake));
    }
}

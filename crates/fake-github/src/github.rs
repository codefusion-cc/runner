use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::httpd::{Handler, Reply, Request, Server};
use crate::keys::test_key;

/// How the fake behaves.
#[derive(Debug, Clone)]
pub struct Options {
    /// The App's client ID, the `iss` of its tokens.
    pub client_id: String,
    /// How long a long poll waits before it answers 202.
    pub poll_timeout: Duration,
    /// How long an installation token lives.
    pub installation_token_ttl: Duration,
    /// How long the Actions service admin token lives.
    pub admin_token_ttl: Duration,
    /// How long a session's message queue token lives.
    pub session_token_ttl: Duration,
    /// Like the real service, assign queued jobs to a scale set by themselves while its long poll says it has room
    /// (`x-scalesetmaxcapacity` above the jobs it has): they arrive as `JobAssigned` and never as `JobAvailable`.
    /// Off, every job is only offered and has to be acquired.
    pub auto_assign: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            client_id: "Iv23test".into(),
            poll_timeout: Duration::from_millis(400),
            installation_token_ttl: Duration::from_secs(3600),
            admin_token_ttl: Duration::from_secs(3600),
            session_token_ttl: Duration::from_secs(3600),
            auto_assign: false,
        }
    }
}

/// Where a job is in its life, as the service tracks it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    /// Offered to every scale set that matches.
    Available,
    /// Taken by a scale set (this id), waiting for its runner.
    Acquired(i64),
    /// Taken by someone else, not a scale set the test watches.
    TakenElsewhere,
    /// Running on this runner.
    Running(String),
    /// Done, with its result.
    Completed(String),
}

#[derive(Debug, Clone)]
struct Job {
    request_id: i64,
    repo: String,
    labels: Vec<String>,
    display_name: String,
    run_id: i64,
    state: JobState,
}

#[derive(Debug, Clone)]
struct Session {
    id: String,
    token: String,
    owner: String,
    serial: u32,
    /// The token is refused until a refresh.
    expired: bool,
    expires_at: i64,
}

#[derive(Debug, Clone)]
struct Msg {
    id: i64,
    /// Job messages of one batch, each a JSON object.
    batch: Vec<Value>,
}

#[derive(Debug)]
struct ScaleSet {
    id: i64,
    repo: String,
    name: String,
    labels: Vec<String>,
    session: Option<Session>,
    queue: VecDeque<Msg>,
    next_message: i64,
    deleted: bool,
}

#[derive(Debug, Clone)]
struct Runner {
    id: i64,
    name: String,
    scale_set: i64,
}

#[derive(Debug, Clone)]
struct Repo {
    private: bool,
    workflows: BTreeMap<String, String>,
    variables: BTreeMap<String, String>,
}

struct Fault {
    matcher: String,
    reply: Reply,
    remaining: usize,
}

type Hook = Box<dyn FnOnce(&FakeGithub) + Send>;

/// A request the fake saw, for assertions.
#[derive(Debug, Clone)]
pub struct Logged {
    /// `METHOD path`.
    pub line: String,
    /// The Authorization header, as sent.
    pub authorization: Option<String>,
    /// The request body.
    pub body: String,
    /// The `x-scalesetmaxcapacity` header of polls.
    pub max_capacity: Option<String>,
    /// The `lastMessageId` of polls.
    pub last_message_id: Option<String>,
}

/// A scale set as the service has it.
#[derive(Debug, Clone)]
pub struct ScaleSetInfo {
    /// Its ID.
    pub id: i64,
    /// Its repository.
    pub repo: String,
    /// Its name.
    pub name: String,
    /// Its labels, lowercased.
    pub labels: Vec<String>,
    /// Whether a message session is open on it.
    pub has_session: bool,
    /// Messages waiting in its queue.
    pub queued_messages: usize,
}

/// A scale set's job counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Jobs offered and not taken.
    pub available: usize,
    /// Jobs assigned to the scale set that have not completed (running ones included).
    pub assigned: usize,
    /// Jobs running.
    pub running: usize,
}

struct State {
    opts: Options,
    clock_offset: i64,
    next: u32,
    repos: BTreeMap<String, Repo>,
    /// token -> expires_at (epoch seconds)
    installation_tokens: HashMap<String, i64>,
    /// token -> (expires_at, repository)
    registration_tokens: HashMap<String, (i64, String)>,
    admin_tokens: HashMap<String, (i64, String)>,
    scale_sets: Vec<ScaleSet>,
    jobs: Vec<Job>,
    runners: Vec<Runner>,
    next_runner: i64,
    next_job: i64,
    faults: Vec<Fault>,
    hooks: Vec<(String, Hook)>,
    log: Vec<Logged>,
    base: String,
}

struct Inner {
    state: Mutex<State>,
    notify: Notify,
}

/// The fake GitHub. Cloning gives another handle to the same fake; the server stops when the last handle that
/// started it is dropped.
#[derive(Clone)]
pub struct FakeGithub {
    inner: Arc<Inner>,
    _server: Option<Arc<Server>>,
}

fn epoch_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
}

fn b64url(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn rfc3339(epoch: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(epoch)
        .expect("time")
        .format(&time::format_description::well_known::Rfc3339)
        .expect("format")
}

/// IMF-fixdate, the form of an HTTP `Date` header.
fn http_date(epoch: i64) -> String {
    const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let t = time::OffsetDateTime::from_unix_timestamp(epoch).expect("time");
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[t.weekday().number_days_from_monday() as usize],
        t.day(),
        MONTHS[t.month() as usize - 1],
        t.year(),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn json_error(status: u16, kind: &str, message: &str) -> Reply {
    Reply::status_json(status, &json!({"typeName": kind, "message": message}))
}

impl State {
    fn now(&self) -> i64 {
        epoch_now() + self.clock_offset
    }

    fn token(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}_{:04}", self.next)
    }

    fn scale_set(&mut self, id: i64) -> Option<&mut ScaleSet> {
        self.scale_sets.iter_mut().find(|s| s.id == id && !s.deleted)
    }

    fn stats(&self, id: i64) -> Stats {
        let Some(set) = self.scale_sets.iter().find(|s| s.id == id) else {
            return Stats {
                available: 0,
                assigned: 0,
                running: 0,
            };
        };
        let mut s = Stats {
            available: 0,
            assigned: 0,
            running: 0,
        };
        for job in &self.jobs {
            match &job.state {
                JobState::Available if matches(&set.labels, &job.labels) && job.repo == set.repo => {
                    s.available += 1
                }
                JobState::Acquired(sid) if *sid == id => s.assigned += 1,
                JobState::Running(runner)
                    if self
                        .runners
                        .iter()
                        .any(|r| &r.name == runner && r.scale_set == id) =>
                {
                    s.assigned += 1;
                    s.running += 1;
                }
                _ => {}
            }
        }
        s
    }

    fn stats_json(&self, id: i64) -> Value {
        let s = self.stats(id);
        json!({
            "totalAvailableJobs": s.available, "totalAcquiredJobs": s.assigned - s.running,
            "totalAssignedJobs": s.assigned, "totalRunningJobs": s.running,
            "totalRegisteredRunners": self.runners.iter().filter(|r| r.scale_set == id).count(),
            "totalBusyRunners": s.running,
            "totalIdleRunners": self.runners.iter().filter(|r| r.scale_set == id).count().saturating_sub(s.running),
        })
    }

    fn push(&mut self, set_id: i64, batch: Vec<Value>) {
        if let Some(set) = self.scale_set(set_id) {
            set.next_message += 1;
            let id = set.next_message;
            set.queue.push_back(Msg { id, batch });
        }
    }

    fn job_message(&self, kind: &str, job: &Job, extra: Value) -> Value {
        let (owner, name) = job.repo.split_once('/').unwrap_or(("", &job.repo));
        let mut v = json!({
            "messageType": kind, "runnerRequestId": job.request_id, "repositoryName": name, "ownerName": owner,
            "jobId": format!("job-{}", job.request_id),
            "jobWorkflowRef": format!("{}/.github/workflows/ci.yml@refs/heads/main", job.repo),
            "jobDisplayName": job.display_name, "workflowRunId": job.run_id, "eventName": "push",
            "requestLabels": job.labels, "queueTime": "2026-10-10T12:00:00Z",
            "scaleSetAssignTime": "2026-10-10T12:00:05Z", "runnerAssignTime": "2026-10-10T12:00:10Z",
            "finishTime": "2026-10-10T12:05:00Z",
        });
        if kind == "JobAssigned" {
            // The real service sends no request ID and no times for a job it assigned.
            v["runnerRequestId"] = json!(0);
            v["queueTime"] = json!("0001-01-01T00:00:00Z");
            v["runnerAssignTime"] = json!("0001-01-01T00:00:00Z");
            v["finishTime"] = json!("0001-01-01T00:00:00Z");
        }
        if let (Some(obj), Some(more)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in more {
                obj.insert(k.clone(), val.clone());
            }
        }
        v
    }

    fn eligible_sets(&self, job: &Job) -> Vec<i64> {
        self.scale_sets
            .iter()
            .filter(|s| !s.deleted && s.repo == job.repo && matches(&s.labels, &job.labels))
            .map(|s| s.id)
            .collect()
    }
}

/// A scale set takes a job when it carries every label the job asks for.
fn matches(set_labels: &[String], job_labels: &[String]) -> bool {
    job_labels.iter().all(|l| set_labels.iter().any(|s| s == l))
}

impl FakeGithub {
    /// Starts a fake GitHub on a free local port.
    pub async fn start(opts: Options) -> FakeGithub {
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                opts,
                clock_offset: 0,
                next: 0,
                repos: BTreeMap::new(),
                installation_tokens: HashMap::new(),
                registration_tokens: HashMap::new(),
                admin_tokens: HashMap::new(),
                scale_sets: vec![],
                jobs: vec![],
                runners: vec![],
                next_runner: 100,
                next_job: 1000,
                faults: vec![],
                hooks: vec![],
                log: vec![],
                base: String::new(),
            }),
            notify: Notify::new(),
        });
        let handle = FakeGithub {
            inner: inner.clone(),
            _server: None,
        };
        let h = handle.clone();
        let handler: Handler = Arc::new(move |req| {
            let h = h.clone();
            Box::pin(async move { h.handle(req).await })
        });
        let server = Arc::new(Server::start(handler).await);
        inner.state.lock().expect("state").base = server.url();
        FakeGithub {
            inner,
            _server: Some(server),
        }
    }

    /// The server's address.
    pub fn url(&self) -> String {
        self.lock().base.clone()
    }

    /// The App's client ID.
    pub fn client_id(&self) -> String {
        self.lock().opts.client_id.clone()
    }

    /// The App's private key as PEM (a throwaway made for the test process).
    pub fn private_key_pem(&self) -> String {
        test_key().private_pem.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().expect("state")
    }

    // ---- setup ----

    /// Adds a repository the App's one installation (ID 1) covers.
    pub fn add_repo(&self, full_name: &str, private: bool) {
        self.lock().repos.insert(
            full_name.to_string(),
            Repo {
                private,
                workflows: BTreeMap::new(),
                variables: BTreeMap::new(),
            },
        );
    }

    /// Sets a workflow file of a repository.
    pub fn set_workflow(&self, repo: &str, file: &str, content: &str) {
        if let Some(r) = self.lock().repos.get_mut(repo) {
            r.workflows.insert(file.to_string(), content.to_string());
        }
    }

    /// Sets a repository variable.
    pub fn set_variable(&self, repo: &str, name: &str, value: &str) {
        if let Some(r) = self.lock().repos.get_mut(repo) {
            r.variables.insert(name.to_string(), value.to_string());
        }
    }

    /// Makes GitHub's clock run this many seconds ahead of the machine's (negative: behind).
    pub fn set_clock_offset(&self, seconds: i64) {
        self.lock().clock_offset = seconds;
    }

    /// Changes how long the admin token lives from now on.
    pub fn set_admin_token_ttl(&self, ttl: Duration) {
        self.lock().opts.admin_token_ttl = ttl;
    }

    /// Changes how long a session's token lives from now on.
    pub fn set_session_token_ttl(&self, ttl: Duration) {
        self.lock().opts.session_token_ttl = ttl;
    }

    /// Changes how long a long poll waits.
    pub fn set_poll_timeout(&self, timeout: Duration) {
        self.lock().opts.poll_timeout = timeout;
    }

    // ---- faults and hooks ----

    /// Answers the next `times` requests whose `METHOD path` contains `matcher` with `reply`, before anything else.
    pub fn fail(&self, matcher: &str, reply: Reply, times: usize) {
        self.lock().faults.push(Fault {
            matcher: matcher.to_string(),
            reply,
            remaining: times,
        });
    }

    /// Runs `hook` once, just before the next request whose `METHOD path` contains `matcher` is handled.
    pub fn before(&self, matcher: &str, hook: impl FnOnce(&FakeGithub) + Send + 'static) {
        self.lock().hooks.push((matcher.to_string(), Box::new(hook)));
    }

    /// Makes a scale set's session token refused until the client refreshes the session.
    pub fn expire_session_token(&self, scale_set: i64) {
        if let Some(Some(s)) = self.lock().scale_set(scale_set).map(|s| s.session.as_mut()) {
            s.expired = true;
        }
    }

    /// Forgets a scale set's session on the service: the next poll finds none.
    pub fn lose_session(&self, scale_set: i64) {
        if let Some(s) = self.lock().scale_set(scale_set) {
            s.session = None;
        }
        self.inner.notify.notify_waiters();
    }

    // ---- jobs ----

    /// Queues a job asking for these labels; every matching scale set is offered it. Returns its request ID.
    pub fn queue_job(&self, repo: &str, labels: &[&str], display_name: &str) -> i64 {
        let mut st = self.lock();
        st.next_job += 1;
        let job = Job {
            request_id: st.next_job,
            repo: repo.to_string(),
            labels: labels.iter().map(|l| l.to_lowercase()).collect(),
            display_name: display_name.to_string(),
            run_id: 7000 + st.next_job,
            state: JobState::Available,
        };
        let id = job.request_id;
        let sets = st.eligible_sets(&job);
        for set in sets {
            let msg = st.job_message(
                "JobAvailable",
                &job,
                json!({"acquireJobUrl": "https://unused.invalid"}),
            );
            st.push(set, vec![msg]);
        }
        st.jobs.push(job);
        drop(st);
        self.inner.notify.notify_waiters();
        id
    }

    /// Another runner takes the job before this one's scale sets do.
    pub fn take_job(&self, request_id: i64) {
        if let Some(j) = self.lock().jobs.iter_mut().find(|j| j.request_id == request_id) {
            j.state = JobState::TakenElsewhere;
        }
        self.inner.notify.notify_waiters();
    }

    /// The runner with this name takes its job and starts it.
    pub fn start_job(&self, runner_name: &str) {
        let mut st = self.lock();
        let Some(runner) = st.runners.iter().find(|r| r.name == runner_name).cloned() else {
            return;
        };
        let Some(i) = st
            .jobs
            .iter()
            .position(|j| j.state == JobState::Acquired(runner.scale_set))
        else {
            return;
        };
        st.jobs[i].state = JobState::Running(runner.name.clone());
        let job = st.jobs[i].clone();
        let msg = st.job_message(
            "JobStarted",
            &job,
            json!({"runnerId": runner.id, "runnerName": runner.name}),
        );
        st.push(runner.scale_set, vec![msg]);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// The job of the runner with this name completes with a result (`succeeded`, `failed`, ...). A one-job runner
    /// unregisters itself, as GitHub's does.
    pub fn complete_job(&self, runner_name: &str, result: &str) {
        let mut st = self.lock();
        let Some(runner) = st.runners.iter().find(|r| r.name == runner_name).cloned() else {
            return;
        };
        let Some(i) = st
            .jobs
            .iter()
            .position(|j| matches!(&j.state, JobState::Running(n) if n == runner_name))
        else {
            return;
        };
        st.jobs[i].state = JobState::Completed(result.to_string());
        let job = st.jobs[i].clone();
        st.runners.retain(|r| r.name != runner_name);
        let msg = st.job_message(
            "JobCompleted",
            &job,
            json!({"result": result, "runnerId": runner.id, "runnerName": runner.name}),
        );
        st.push(runner.scale_set, vec![msg]);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// The job is cancelled. One a scale set had acquired completes as `canceled` with no runner.
    pub fn cancel_job(&self, request_id: i64) {
        let mut st = self.lock();
        let Some(i) = st.jobs.iter().position(|j| j.request_id == request_id) else {
            return;
        };
        let job = st.jobs[i].clone();
        match job.state {
            JobState::Acquired(set) => {
                st.jobs[i].state = JobState::Completed("canceled".into());
                let msg = st.job_message("JobCompleted", &job, json!({"result": "canceled"}));
                st.push(set, vec![msg]);
            }
            JobState::Available => st.jobs[i].state = JobState::Completed("canceled".into()),
            _ => {}
        }
        drop(st);
        self.inner.notify.notify_waiters();
    }

    // ---- inspection ----

    /// Every scale set that exists.
    pub fn scale_sets(&self) -> Vec<ScaleSetInfo> {
        self.lock()
            .scale_sets
            .iter()
            .filter(|s| !s.deleted)
            .map(|s| ScaleSetInfo {
                id: s.id,
                repo: s.repo.clone(),
                name: s.name.clone(),
                labels: s.labels.clone(),
                has_session: s.session.is_some(),
                queued_messages: s.queue.len(),
            })
            .collect()
    }

    /// The names of the runners currently registered.
    pub fn runners(&self) -> Vec<String> {
        self.lock().runners.iter().map(|r| r.name.clone()).collect()
    }

    /// A job's state.
    pub fn job_state(&self, request_id: i64) -> Option<JobState> {
        self.lock()
            .jobs
            .iter()
            .find(|j| j.request_id == request_id)
            .map(|j| j.state.clone())
    }

    /// A scale set's counts.
    pub fn stats(&self, scale_set: i64) -> Stats {
        self.lock().stats(scale_set)
    }

    /// Every request seen so far.
    pub fn requests(&self) -> Vec<Logged> {
        self.lock().log.clone()
    }

    /// How many requests so far had a `METHOD path` containing `needle`.
    pub fn count(&self, needle: &str) -> usize {
        self.lock().log.iter().filter(|l| l.line.contains(needle)).count()
    }

    // ---- the server ----

    async fn handle(&self, req: Request) -> Reply {
        let line = req.line();
        let hook = {
            let mut st = self.lock();
            st.log.push(Logged {
                line: line.clone(),
                authorization: req.header("authorization").map(String::from),
                body: String::from_utf8_lossy(&req.body).to_string(),
                max_capacity: req.header("x-scalesetmaxcapacity").map(String::from),
                last_message_id: req.query("lastMessageId").map(String::from),
            });
            st.hooks
                .iter()
                .position(|(m, _)| line.contains(m))
                .map(|i| st.hooks.remove(i).1)
        };
        if let Some(hook) = hook {
            hook(self);
        }
        {
            let mut st = self.lock();
            if let Some(i) = st
                .faults
                .iter()
                .position(|f| f.remaining > 0 && line.contains(&f.matcher))
            {
                st.faults[i].remaining -= 1;
                return st.faults[i].reply.clone();
            }
        }
        let reply = self.route(req).await;
        let date = http_date(self.lock().now());
        reply.with_header("date", &date)
    }

    async fn route(&self, req: Request) -> Reply {
        let path = req.path.clone();
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let m = req.method.as_str();
        match (m, segs.as_slice()) {
            ("GET", ["app", "installations"]) => self.with_jwt(&req, |_| Reply::json(&json!([{"id": 1, "account": {"login": "org"}}]))),
            ("POST", ["app", "installations", id, "access_tokens"]) => {
                let id = id.to_string();
                self.with_jwt(&req, |st| {
                    if id != "1" {
                        return Reply::status_json(404, &json!({"message": "Not Found"}));
                    }
                    let token = st.token("ghs");
                    let exp = st.now() + st.opts.installation_token_ttl.as_secs() as i64;
                    st.installation_tokens.insert(token.clone(), exp);
                    Reply::status_json(201, &json!({"token": token, "expires_at": rfc3339(exp)}))
                })
            }
            ("GET", ["installation", "repositories"]) => self.with_installation(&req, |st| {
                let repos: Vec<Value> = st
                    .repos
                    .iter()
                    .map(|(name, r)| {
                        json!({"full_name": name, "private": r.private, "archived": false, "disabled": false,
                               "pushed_at": "2026-10-10T10:00:00Z", "html_url": format!("https://github.com/{name}")})
                    })
                    .collect();
                Reply::json(&json!({"total_count": repos.len(), "repositories": repos}))
            }),
            ("GET", ["repos", owner, repo, "installation"]) => {
                let name = format!("{owner}/{repo}");
                self.with_jwt(&req, |st| {
                    if st.repos.contains_key(&name) {
                        Reply::json(&json!({"id": 1}))
                    } else {
                        Reply::status_json(404, &json!({"message": "Not Found"}))
                    }
                })
            }
            ("GET", ["repos", owner, repo, "contents", ".github", "workflows"]) => {
                let name = format!("{owner}/{repo}");
                self.with_installation(&req, |st| match st.repos.get(&name) {
                    Some(r) if !r.workflows.is_empty() => Reply::json(&Value::Array(
                        r.workflows
                            .iter()
                            .map(|(file, content)| json!({"name": file, "type": "file", "sha": sha_of(content)}))
                            .collect(),
                    )),
                    _ => Reply::status_json(404, &json!({"message": "Not Found"})),
                })
            }
            ("GET", ["repos", owner, repo, "git", "blobs", sha]) => {
                let name = format!("{owner}/{repo}");
                let sha = sha.to_string();
                self.with_installation(&req, |st| {
                    let found = st.repos.get(&name).and_then(|r| r.workflows.values().find(|c| sha_of(c) == sha));
                    match found {
                        Some(content) => {
                            let wrapped: Vec<String> = STANDARD
                                .encode(content)
                                .as_bytes()
                                .chunks(60)
                                .map(|c| String::from_utf8_lossy(c).to_string())
                                .collect();
                            Reply::json(&json!({"content": wrapped.join("\n") + "\n", "encoding": "base64"}))
                        }
                        None => Reply::status_json(404, &json!({"message": "Not Found"})),
                    }
                })
            }
            ("GET", ["repos", owner, repo, "actions", "variables"]) => {
                let name = format!("{owner}/{repo}");
                self.with_installation(&req, |st| match st.repos.get(&name) {
                    Some(r) => {
                        let vars: Vec<Value> = r.variables.iter().map(|(k, v)| json!({"name": k, "value": v})).collect();
                        Reply::json(&json!({"total_count": vars.len(), "variables": vars}))
                    }
                    None => Reply::status_json(404, &json!({"message": "Not Found"})),
                })
            }
            ("GET", ["repos", _, _, "actions", "organization-variables"]) => {
                self.with_installation(&req, |_| Reply::json(&json!({"total_count": 0, "variables": []})))
            }
            ("POST", ["repos", owner, repo, "actions", "runners", "registration-token"]) => {
                let name = format!("{owner}/{repo}");
                self.with_installation(&req, |st| {
                    if !st.repos.contains_key(&name) {
                        return Reply::status_json(404, &json!({"message": "Not Found"}));
                    }
                    let token = st.token("AREG");
                    let exp = st.now() + 3600;
                    st.registration_tokens.insert(token.clone(), (exp, name.clone()));
                    Reply::status_json(201, &json!({"token": token, "expires_at": rfc3339(exp)}))
                })
            }
            ("POST", ["actions", "runner-registration"]) => self.runner_registration(&req),
            (_, ["svc", rest @ ..]) => self.actions_service(&req, rest).await,
            _ => Reply::status_json(404, &json!({"message": format!("fake GitHub has no {} {}", req.method, req.path)})),
        }
    }

    fn unauthorized(&self, st: &State, message: &str) -> Reply {
        let _ = st;
        Reply::status_json(401, &json!({"message": message}))
    }

    /// Checks an App JWT as GitHub does: signature, issuer, not issued in its future, not valid for over ten minutes.
    fn with_jwt(&self, req: &Request, f: impl FnOnce(&mut State) -> Reply) -> Reply {
        let mut st = self.lock();
        let Some(token) = req
            .header("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
        else {
            return self.unauthorized(&st, "A JSON web token could not be decoded");
        };
        match verify_jwt(token, &st.opts.client_id, st.now()) {
            Ok(()) => f(&mut st),
            Err(why) => self.unauthorized(&st, &why),
        }
    }

    fn with_installation(&self, req: &Request, f: impl FnOnce(&mut State) -> Reply) -> Reply {
        let mut st = self.lock();
        let token = req
            .header("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("");
        match st.installation_tokens.get(token) {
            Some(exp) if *exp > st.now() => f(&mut st),
            _ => self.unauthorized(&st, "Bad credentials"),
        }
    }

    fn runner_registration(&self, req: &Request) -> Reply {
        let mut st = self.lock();
        let token = req
            .header("authorization")
            .and_then(|h| h.strip_prefix("RemoteAuth "))
            .unwrap_or("");
        let repo = match st.registration_tokens.get(token) {
            Some((exp, repo)) if *exp > st.now() => repo.clone(),
            _ => return Reply::status_json(401, &json!({"message": "Bad credentials"})),
        };
        let exp = st.now() + st.opts.admin_token_ttl.as_secs() as i64;
        st.next += 1;
        let payload = json!({"exp": exp, "n": st.next}).to_string();
        let admin = format!(
            "{}.{}.{}",
            b64url(br#"{"alg":"RS256"}"#),
            b64url(payload.as_bytes()),
            "sig"
        );
        st.admin_tokens.insert(admin.clone(), (exp, repo));
        Reply::json(&json!({"url": format!("{}/svc", st.base), "token": admin}))
    }

    async fn actions_service(&self, req: &Request, segs: &[&str]) -> Reply {
        // Message queue calls carry the session token; everything else the admin token.
        let bearer = req
            .header("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();
        let m = req.method.as_str();
        // /svc/_apis/runtime/runnerscalesets[/id[/sessions[/sid] | /acquirejobs | /generatejitconfig | /messages[/mid]]]
        if let ["_apis", "runtime", "runnerscalesets", rest @ ..] = segs {
            match (m, rest) {
                ("GET", []) => return self.admin(&bearer, |st, repo| list_scale_sets(st, req, repo)),
                ("POST", []) => return self.admin(&bearer, |st, repo| create_scale_set(st, req, repo)),
                ("GET", [id]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    return self.admin(&bearer, |st, _| {
                        match st.scale_sets.iter().find(|s| s.id == id && !s.deleted) {
                            Some(s) => Reply::json(&scale_set_json(st, s)),
                            None => json_error(404, "RunnerScaleSetNotFoundException", "scale set not found"),
                        }
                    });
                }
                ("PATCH", [id]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    return self.admin(&bearer, |st, _| update_scale_set(st, id, req));
                }
                ("DELETE", [id]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    return self.admin(&bearer, |st, _| match st.scale_set(id) {
                        Some(s) => {
                            s.deleted = true;
                            s.session = None;
                            Reply::empty(204)
                        }
                        None => json_error(404, "RunnerScaleSetNotFoundException", "scale set not found"),
                    });
                }
                ("POST", [id, "sessions"]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    return self.admin(&bearer, |st, _| create_session(st, id, req));
                }
                ("PATCH", [id, "sessions", sid]) => {
                    let (id, sid): (i64, String) = (id.parse().unwrap_or(0), sid.to_string());
                    return self.admin(&bearer, |st, _| refresh_session(st, id, &sid));
                }
                ("DELETE", [id, "sessions", sid]) => {
                    let (id, sid): (i64, String) = (id.parse().unwrap_or(0), sid.to_string());
                    return self.admin(&bearer, |st, _| match st.scale_set(id) {
                        Some(s) if s.session.as_ref().is_some_and(|x| x.id == sid) => {
                            s.session = None;
                            Reply::empty(204)
                        }
                        _ => json_error(404, "SessionNotFoundException", "session not found"),
                    });
                }
                ("POST", [id, "generatejitconfig"]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    return self.admin(&bearer, |st, _| generate_jit(st, id, req));
                }
                ("POST", [id, "acquirejobs"]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    let reply = self.session_auth(id, &bearer, |st| acquire_jobs(st, id, req));
                    self.inner.notify.notify_waiters();
                    return reply;
                }
                ("GET", [id, "messages"]) => {
                    let id: i64 = id.parse().unwrap_or(0);
                    return self.poll(id, &bearer, req).await;
                }
                ("DELETE", [id, "messages", mid]) => {
                    let (id, mid): (i64, i64) = (id.parse().unwrap_or(0), mid.parse().unwrap_or(0));
                    return self.session_auth(id, &bearer, |st| {
                        if let Some(s) = st.scale_set(id) {
                            s.queue.retain(|msg| msg.id != mid);
                        }
                        Reply::empty(204)
                    });
                }
                _ => {}
            }
        }
        if let ("DELETE", ["_apis", "distributedtask", "pools", "0", "agents", id]) = (m, segs) {
            let id: i64 = id.parse().unwrap_or(0);
            return self.admin(&bearer, |st, _| {
                let before = st.runners.len();
                st.runners.retain(|r| r.id != id);
                if st.runners.len() == before {
                    json_error(404, "AgentNotFoundException", "runner not found")
                } else {
                    Reply::empty(204)
                }
            });
        }
        Reply::status_json(
            404,
            &json!({"message": format!("fake GitHub has no {} {}", req.method, req.path)}),
        )
    }

    fn admin(&self, bearer: &str, f: impl FnOnce(&mut State, &str) -> Reply) -> Reply {
        let mut st = self.lock();
        match st.admin_tokens.get(bearer).cloned() {
            Some((exp, repo)) if exp > st.now() => f(&mut st, &repo),
            _ => json_error(401, "VssUnauthorizedException", "admin token invalid or expired"),
        }
    }

    fn session_auth(&self, id: i64, bearer: &str, f: impl FnOnce(&mut State) -> Reply) -> Reply {
        let mut st = self.lock();
        let now = st.now();
        let Some(set) = st.scale_set(id) else {
            return json_error(404, "RunnerScaleSetNotFoundException", "scale set not found");
        };
        match &set.session {
            None => json_error(404, "MessageQueueNotFoundException", "no session"),
            Some(s) if s.token != bearer || s.expired || s.expires_at <= now => {
                json_error(401, "MessageQueueTokenExpiredException", "session token expired")
            }
            Some(_) => f(&mut st),
        }
    }

    async fn poll(&self, id: i64, bearer: &str, req: &Request) -> Reply {
        let last: i64 = req
            .query("lastMessageId")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let capacity: Option<usize> = req.header("x-scalesetmaxcapacity").and_then(|v| v.parse().ok());
        let deadline = tokio::time::Instant::now() + self.lock().opts.poll_timeout;
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let answer = self.session_auth(id, bearer, |st| {
                if let Some(room) = capacity {
                    auto_assign(st, id, room);
                }
                let front = st
                    .scale_sets
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| s.queue.iter().find(|msg| msg.id > last).cloned());
                match front {
                    Some(msg) => {
                        let body = Value::Array(msg.batch.clone()).to_string();
                        Reply::json(&json!({
                            "messageId": msg.id, "messageType": "RunnerScaleSetJobMessages", "body": body,
                            "statistics": st.stats_json(id),
                        }))
                    }
                    None => Reply::empty(0),
                }
            });
            match answer {
                Reply::Response { status: 0, .. } => {}
                other => return other,
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return Reply::empty(202);
            }
        }
    }
}

/// Assigns queued jobs to the scale set up to `capacity` runners, as the service does for a poll that says it has
/// room: the offers for them are withdrawn and a `JobAssigned` replaces each.
fn auto_assign(st: &mut State, id: i64, capacity: usize) {
    if !st.opts.auto_assign {
        return;
    }
    let Some(set) = st.scale_sets.iter().find(|s| s.id == id && !s.deleted) else {
        return;
    };
    let (labels, repo) = (set.labels.clone(), set.repo.clone());
    let mut room = capacity.saturating_sub(st.stats(id).assigned);
    let wanted: Vec<usize> = (0..st.jobs.len())
        .filter(|&i| {
            let j = &st.jobs[i];
            j.state == JobState::Available && j.repo == repo && matches(&labels, &j.labels)
        })
        .collect();
    for i in wanted {
        if room == 0 {
            break;
        }
        room -= 1;
        st.jobs[i].state = JobState::Acquired(id);
        let job = st.jobs[i].clone();
        if let Some(set) = st.scale_set(id) {
            set.queue.retain(|m| {
                !m.batch
                    .iter()
                    .any(|v| v["messageType"] == "JobAvailable" && v["runnerRequestId"] == job.request_id)
            });
        }
        let msg = st.job_message("JobAssigned", &job, json!({}));
        st.push(id, vec![msg]);
    }
}

fn sha_of(content: &str) -> String {
    // Not a git hash; unique per content, which is all the client needs.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in content.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}{:08x}", content.len())
}

fn verify_jwt(token: &str, client_id: &str, now: i64) -> Result<(), String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("A JSON web token could not be decoded".into());
    }
    let signing_input = format!("{}.{}", parts[0], parts[1]);
    let sig = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| "A JSON web token could not be decoded".to_string())?;
    let key = ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        test_key().public_der.as_slice(),
    );
    key.verify(signing_input.as_bytes(), &sig)
        .map_err(|_| "Bad credentials: the JSON web token signature is not valid".to_string())?;
    let header: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap_or_default()).unwrap_or_default();
    if header["alg"] != "RS256" {
        return Err("A JSON web token must be signed with RS256".into());
    }
    let claims: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap_or_default()).unwrap_or_default();
    if claims["iss"] != client_id {
        return Err("'Issuer' claim ('iss') does not name this App".into());
    }
    let (Some(iat), Some(exp)) = (claims["iat"].as_i64(), claims["exp"].as_i64()) else {
        return Err("'iat' and 'exp' must be integers".into());
    };
    if iat > now {
        return Err("'Issued at' claim ('iat') must be an Integer representing the time that the assertion was issued".into());
    }
    if exp <= now {
        return Err("'Expiration time' claim ('exp') must be a numeric value representing the future time at which the assertion expires".into());
    }
    if exp - now > 600 {
        return Err("'Expiration time' claim ('exp') is too far in the future".into());
    }
    Ok(())
}

fn scale_set_json(st: &State, s: &ScaleSet) -> Value {
    json!({
        "id": s.id, "name": s.name, "runnerGroupId": 1, "runnerGroupName": "Default",
        "labels": s.labels.iter().map(|l| json!({"type": "System", "name": l})).collect::<Vec<_>>(),
        "RunnerSetting": {"disableUpdate": true}, "createdOn": "2026-10-10T10:00:00Z",
        "statistics": st.stats_json(s.id),
    })
}

fn list_scale_sets(st: &mut State, req: &Request, repo: &str) -> Reply {
    let name = req.query("name").map(String::from);
    let found: Vec<Value> = st
        .scale_sets
        .iter()
        .filter(|s| !s.deleted && s.repo == repo && name.as_ref().is_none_or(|n| &s.name == n))
        .map(|s| scale_set_json(st, s))
        .collect();
    Reply::json(&json!({"count": found.len(), "value": found}))
}

fn create_scale_set(st: &mut State, req: &Request, repo: &str) -> Reply {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let name = body["name"].as_str().unwrap_or_default().to_string();
    if name.is_empty() {
        return json_error(400, "InvalidRunnerScaleSetException", "name required");
    }
    if st
        .scale_sets
        .iter()
        .any(|s| !s.deleted && s.repo == repo && s.name == name)
    {
        return json_error(
            409,
            "RunnerScaleSetExistsException",
            "a scale set with this name exists",
        );
    }
    let labels: Vec<String> = body["labels"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|l| l["name"].as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default();
    let id = 10 + st.scale_sets.len() as i64;
    st.scale_sets.push(ScaleSet {
        id,
        repo: repo.to_string(),
        name,
        labels,
        session: None,
        queue: VecDeque::new(),
        next_message: 0,
        deleted: false,
    });
    let s = st.scale_sets.last().expect("just pushed");
    Reply::json(&scale_set_json(st, s))
}

fn update_scale_set(st: &mut State, id: i64, req: &Request) -> Reply {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let labels: Option<Vec<String>> = body["labels"].as_array().map(|a| {
        a.iter()
            .filter_map(|l| l["name"].as_str().map(|s| s.to_lowercase()))
            .collect()
    });
    match st.scale_set(id) {
        Some(s) => {
            if let Some(l) = labels {
                s.labels = l;
            }
            let s = st.scale_sets.iter().find(|s| s.id == id).expect("exists");
            Reply::json(&scale_set_json(st, s))
        }
        None => json_error(404, "RunnerScaleSetNotFoundException", "scale set not found"),
    }
}

fn session_json(st: &State, set_id: i64, s: &Session) -> Value {
    json!({
        "sessionId": s.id, "ownerName": s.owner,
        "messageQueueUrl": format!("{}/svc/_apis/runtime/runnerscalesets/{}/messages", st.base, set_id),
        "messageQueueAccessToken": s.token, "statistics": st.stats_json(set_id),
    })
}

fn create_session(st: &mut State, id: i64, req: &Request) -> Reply {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let owner = body["ownerName"].as_str().unwrap_or_default().to_string();
    let now = st.now();
    let ttl = st.opts.session_token_ttl.as_secs() as i64;
    let token = st.token("mq");
    let sid = st.token("sess");
    let Some(set) = st.scale_set(id) else {
        return json_error(404, "RunnerScaleSetNotFoundException", "scale set not found");
    };
    if set.session.is_some() {
        return json_error(
            409,
            "RunnerScaleSetSessionExistsException",
            "a session exists for this scale set",
        );
    }
    let session = Session {
        id: sid,
        token,
        owner,
        serial: 0,
        expired: false,
        expires_at: now + ttl,
    };
    set.session = Some(session.clone());
    Reply::json(&session_json(st, id, &session))
}

fn refresh_session(st: &mut State, id: i64, sid: &str) -> Reply {
    let now = st.now();
    let ttl = st.opts.session_token_ttl.as_secs() as i64;
    let token = st.token("mq");
    let Some(set) = st.scale_set(id) else {
        return json_error(404, "RunnerScaleSetNotFoundException", "scale set not found");
    };
    match &mut set.session {
        Some(s) if s.id == sid => {
            s.token = token;
            s.serial += 1;
            s.expired = false;
            s.expires_at = now + ttl;
            let s = s.clone();
            Reply::json(&session_json(st, id, &s))
        }
        _ => json_error(404, "SessionNotFoundException", "session not found"),
    }
}

fn acquire_jobs(st: &mut State, id: i64, req: &Request) -> Reply {
    let wanted: Vec<i64> = serde_json::from_slice(&req.body).unwrap_or_default();
    let mut got = vec![];
    for rid in wanted {
        let Some(i) = st.jobs.iter().position(|j| j.request_id == rid) else {
            continue;
        };
        if st.jobs[i].state == JobState::Available {
            st.jobs[i].state = JobState::Acquired(id);
            got.push(rid);
            let job = st.jobs[i].clone();
            let msg = st.job_message("JobAssigned", &job, json!({}));
            st.push(id, vec![msg]);
        }
    }
    Reply::json(&json!({"count": got.len(), "value": got}))
}

fn generate_jit(st: &mut State, id: i64, req: &Request) -> Reply {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let name = body["name"].as_str().unwrap_or_default().to_string();
    if name.is_empty() {
        return json_error(400, "InvalidRunnerException", "name required");
    }
    if st.scale_set(id).is_none() {
        return json_error(404, "RunnerScaleSetNotFoundException", "scale set not found");
    }
    if st.runners.iter().any(|r| r.name == name) {
        return json_error(409, "AgentExistsException", "a runner with this name exists");
    }
    st.next_runner += 1;
    let rid = st.next_runner;
    st.runners.push(Runner {
        id: rid,
        name: name.clone(),
        scale_set: id,
    });
    Reply::json(&json!({
        "runner": {"id": rid, "name": name, "runnerScaleSetId": id},
        "encodedJITConfig": b64url(format!("jit-config-for-{name}").as_bytes()),
    }))
}

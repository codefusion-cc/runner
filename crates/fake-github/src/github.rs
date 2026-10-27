use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
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
    /// Assign queued jobs to scale sets as the real service does, and never more kindly: they arrive as `JobAssigned`,
    /// never as `JobAvailable`. Off, every job is only offered and has to be acquired. The rules, each beside what was
    /// seen at GitHub (the probes ran 2026-10-12 against a throwaway repository, `docs/github-protocol.md`):
    ///
    /// - It looks at its queue when a job is queued, ends, is cancelled or is taken elsewhere. Seen: probes E, I and
    ///   J2 (a job queued, or one GitHub took back, and waiting jobs were assigned within a second); the Mac mini's
    ///   log at 18:52:50 (a job queued, a burst of assignments). That an end makes it look is inferred from the log.
    /// - It looks at no other time unless a test says so ([`FakeGithub::github_looks`]). Seen: GitHub also looks by
    ///   itself, sometimes within 30 s of room (probes J1, I2), sometimes after 2.5 minutes (A, G), sometimes not in
    ///   3 minutes (J2, J3), and not for 13 minutes on the mini (18:39 to 18:52). Nothing the Runner does makes it
    ///   look: not a session opened (probes E, F), not a scale set updated (I1, L), not an Actions runner registered
    ///   (J1 to J3, L), but for one registered outside any scale set that comes online (N,
    ///   [`FakeGithub::runner_online`]).
    /// - It gives each scale set with a session as many jobs as the smaller room of its last two polls' `x-scalesetmaxcapacity`
    ///   leaves. Seen: probe E assigned against the poll before the newest, probe J1 against the newest alone; the
    ///   smaller is no kinder than either. Inferred: which poll GitHub reads.
    /// - The counts never name a job it holds: `totalAvailableJobs` stays 0. Seen: every count in every probe.
    /// - A session that ends, or that it forgets, takes its pending messages with it. Seen: probe D kept one across
    ///   a session's end. Kinder than the fake, which does not; the Runner must not lean on it.
    /// - A refresh of a session (`PATCH`) brings no counts. Seen: probe C (`"statistics": null`).
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
    /// Taken by a scale set (this id), waiting for its Actions runner.
    Acquired(i64),
    /// Taken by someone else, not a scale set the test watches.
    TakenElsewhere,
    /// Running on this Actions runner.
    Running(String),
    /// Done, with its result.
    Completed(String),
}

#[derive(Debug, Clone)]
struct Job {
    request_id: i64,
    repo: String,
    labels: Vec<String>,
    /// The labels as the job's `runs-on` wrote them, which the REST view of its run shows.
    written: Vec<String>,
    /// When it was queued, on GitHub's clock.
    created_at: i64,
    display_name: String,
    /// The ref its workflow runs at, `refs/heads/main` or `refs/pull/<n>/merge`.
    git_ref: String,
    run_id: i64,
    state: JobState,
    /// When it was last handed to a scale set, on the queue's clock ([`State::queue_now`]).
    assigned_at: i64,
}

#[derive(Debug, Clone)]
struct Session {
    id: String,
    token: String,
    owner: String,
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
    /// How many polls came for it. Only the newest is served: the one before it was given up by its listener,
    /// whose closed connection ends it at the service, and offers nothing any more.
    polls: u64,
    /// The room the newest poll said the scale set has.
    capacity: Option<usize>,
    /// The room the poll before it said: the service assigns against the smaller.
    previous_capacity: Option<usize>,
    deleted: bool,
}

#[derive(Debug, Clone)]
struct Runner {
    id: i64,
    name: String,
    /// Its scale set; [`PLAIN`] for one registered outside any.
    scale_set: i64,
    repo: String,
    /// What one registered outside a scale set carries: it takes a queued job of its repository whose labels it has.
    labels: Vec<String>,
}

/// The scale set of an Actions runner registered outside any, with the REST API's `generate-jitconfig`.
const PLAIN: i64 = 0;

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
    /// The query's pairs.
    pub query: Vec<(String, String)>,
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
#[derive(Debug, Clone, Copy, Default)]
struct Stats {
    /// Jobs offered and not taken.
    available: usize,
    /// Jobs assigned to the scale set that have not completed (running ones included).
    assigned: usize,
    /// Jobs running.
    running: usize,
}

struct State {
    opts: Options,
    /// Whether a session's first poll makes the service look at its queue; see
    clock_offset: i64,
    next: u32,
    repos: BTreeMap<String, Repo>,
    /// token -> expires_at (epoch seconds)
    installation_tokens: HashMap<String, i64>,
    /// The installation tokens that only read, as the console's are: GitHub refuses them a write.
    read_only: BTreeSet<String>,
    /// token -> (expires_at, repository)
    registration_tokens: HashMap<String, (i64, String)>,
    admin_tokens: HashMap<String, (i64, String)>,
    scale_sets: Vec<ScaleSet>,
    jobs: Vec<Job>,
    runners: Vec<Runner>,
    /// Actions runners whose registration a DELETE removed, by name.
    removed_at_request: Vec<String>,
    next_runner: i64,
    next_job: i64,
    faults: Vec<Fault>,
    /// (matcher, how many more answers get lost)
    lost: Vec<(String, usize)>,
    /// (matcher, how long, how many more requests wait so long before they are handled)
    delays: Vec<(String, Duration, usize)>,
    /// Where registration and sessions say the service and its message queue are, when not here.
    service_at: Option<String>,
    queue_at: Option<String>,
    hooks: Vec<(String, Hook)>,
    log: Vec<Logged>,
    base: String,
    /// How long a test let pass on the queue's clock beyond the real time ([`FakeGithub::pass`]).
    passed: i64,
    /// Every Actions runner ever registered, by ID, removed or not.
    registered: HashMap<i64, Runner>,
    /// Whether the REST view of an Actions runner answers one read behind ([`FakeGithub::set_rest_stale`]).
    rest_stale: bool,
    /// What the REST view of each Actions runner said at its last read: busy or not, `None` once removed.
    rest_seen: HashMap<i64, Option<bool>>,
    /// Whether every request is dropped unanswered and changes nothing ([`FakeGithub::set_down`]).
    down: bool,
    /// How often a job was handed to a scale set, the same job again after a take-back included.
    assignments: usize,
    /// How many conditional requests were answered 304 Not Modified.
    not_modified: usize,
    /// The runs of a pull request from a fork.
    forks: BTreeSet<i64>,
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

    /// The clock of the queue's own timeouts: the real time plus what a test let pass.
    fn queue_now(&self) -> i64 {
        epoch_now() + self.passed
    }

    fn token(&mut self, prefix: &str) -> String {
        self.next += 1;
        format!("{prefix}_{:04}", self.next)
    }

    /// A token that registers Actions runners with `repo`, and when it expires; none for a repository the App lacks.
    fn registration_token(&mut self, repo: &str) -> Option<(String, i64)> {
        if !self.repos.contains_key(repo) {
            return None;
        }
        let token = self.token("AREG");
        let exp = self.now() + 3600;
        self.registration_tokens
            .insert(token.clone(), (exp, repo.to_string()));
        Some((token, exp))
    }

    fn scale_set(&mut self, id: i64) -> Option<&mut ScaleSet> {
        self.scale_sets.iter_mut().find(|s| s.id == id && !s.deleted)
    }

    fn offers_room(&self, repo: &str) -> bool {
        self.scale_sets.iter().any(|s| {
            s.repo == repo
                && !s.deleted
                && s.session.is_some()
                && s.capacity
                    .zip(s.previous_capacity)
                    .is_some_and(|(newest, before)| newest.min(before) > self.stats(s.id).assigned)
        })
    }

    fn stats(&self, id: i64) -> Stats {
        let Some(set) = self.scale_sets.iter().find(|s| s.id == id) else {
            return Stats::default();
        };
        let mut s = Stats::default();
        for job in &self.jobs {
            match &job.state {
                JobState::Available
                    if !self.opts.auto_assign
                        && matches(&set.labels, &job.labels)
                        && job.repo == set.repo =>
                {
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

    /// Queues a message for the scale set's session. With no session there is nobody to keep it for: it is lost, as
    /// a session's pending messages are when it ends.
    fn push(&mut self, set_id: i64, batch: Vec<Value>) {
        if let Some(set) = self.scale_set(set_id) {
            set.next_message += 1;
            let id = set.next_message;
            if set.session.is_some() {
                set.queue.push_back(Msg { id, batch });
            }
        }
    }

    fn job_message(&self, kind: &str, job: &Job, extra: Value) -> Value {
        let (owner, name) = job.repo.split_once('/').unwrap_or(("", &job.repo));
        let mut v = json!({
            "messageType": kind, "runnerRequestId": job.request_id, "repositoryName": name, "ownerName": owner,
            "jobId": format!("job-{}", job.request_id),
            "jobWorkflowRef": format!("{}/.github/workflows/ci.yml@{}", job.repo, job.git_ref),
            "jobDisplayName": job.display_name, "workflowRunId": job.run_id,
            "eventName": if job.git_ref.starts_with("refs/pull/") { "pull_request" } else { "push" },
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

    /// A run's status as its REST view says it: `queued` while every job waits, `completed` once all ended,
    /// `in_progress` between.
    /// When a run last changed, as its REST view says: a time that moves whenever the status of one of its jobs
    /// does. (GitHub's moves with more; what counts is that it moves with that.)
    fn run_updated(&self, run_id: i64, created: i64) -> i64 {
        let digest = sha_of(&self.run_states(run_id).join(","));
        created + 1 + i64::from_str_radix(&digest[..5], 16).unwrap_or(0)
    }

    /// The REST status of each job of a run.
    fn run_states(&self, run_id: i64) -> Vec<&'static str> {
        self.jobs
            .iter()
            .filter(|j| j.run_id == run_id)
            .map(|j| job_status(&j.state))
            .collect()
    }

    fn run_status(&self, run_id: i64) -> &'static str {
        let states = self.run_states(run_id);
        if states.iter().all(|s| *s == "queued") {
            "queued"
        } else if states.iter().all(|s| *s == "completed") {
            "completed"
        } else {
            "in_progress"
        }
    }

    /// A JSON answer with its ETag, or 304 with no body when the request named that ETag already.
    fn conditional(&mut self, req: &Request, body: &Value) -> Reply {
        let text = serde_json::to_string(body).expect("json");
        let etag = format!("W/\"{}\"", sha_of(&text));
        if req.header("if-none-match") == Some(etag.as_str()) {
            self.not_modified += 1;
            return Reply::empty(304).with_header("etag", &etag);
        }
        Reply::json(body).with_header("etag", &etag)
    }

    fn eligible_sets(&self, job: &Job) -> Vec<i64> {
        self.scale_sets
            .iter()
            .filter(|s| !s.deleted && s.repo == job.repo && matches(&s.labels, &job.labels))
            .map(|s| s.id)
            .collect()
    }
}

/// A job's status as the REST view of its run says it.
fn job_status(state: &JobState) -> &'static str {
    match state {
        JobState::Available | JobState::Acquired(_) => "queued",
        JobState::Running(_) | JobState::TakenElsewhere => "in_progress",
        JobState::Completed(_) => "completed",
    }
}

/// A listing's `per_page`: GitHub's 30 when it names none, 100 at most.
fn page_size(req: &Request) -> usize {
    req.query("per_page")
        .and_then(|n| n.parse::<usize>().ok())
        .unwrap_or(30)
        .clamp(1, 100)
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
                read_only: BTreeSet::new(),
                registration_tokens: HashMap::new(),
                admin_tokens: HashMap::new(),
                scale_sets: vec![],
                jobs: vec![],
                runners: vec![],
                removed_at_request: vec![],
                next_runner: 100,
                next_job: 1000,
                faults: vec![],
                lost: vec![],
                delays: vec![],
                service_at: None,
                queue_at: None,
                hooks: vec![],
                log: vec![],
                base: String::new(),
                passed: 0,
                registered: HashMap::new(),
                rest_stale: false,
                rest_seen: HashMap::new(),
                down: false,
                assignments: 0,
                not_modified: 0,
                forks: BTreeSet::new(),
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

    /// Changes how long a long poll waits.
    pub fn set_poll_timeout(&self, timeout: Duration) {
        self.lock().opts.poll_timeout = timeout;
    }

    /// Lets time pass on the queue's clock: an assignment no Actions runner took within [`TAKE_BACK_AFTER`] is taken
    /// back. Tokens and sessions keep the real time.
    pub fn pass(&self, time: Duration) {
        let mut st = self.lock();
        st.passed += i64::try_from(time.as_secs()).unwrap_or(i64::MAX);
        if take_back(&mut st) {
            look_at_the_queue(&mut st);
        }
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// Makes the REST view of an Actions runner answer one read behind: what the read before it saw, and for the
    /// first read the view as the registration left it (registered, idle). Off, it answers what is true now.
    pub fn set_rest_stale(&self, stale: bool) {
        self.lock().rest_stale = stale;
    }

    /// Takes the fake down: every request is dropped unanswered and changes nothing, as for a client whose process
    /// died. Up again with `false`.
    pub fn set_down(&self, down: bool) {
        self.lock().down = down;
    }

    // ---- what the console does with the App's key ----

    /// The App's installations, each with its account, as `GET /app/installations` lists them.
    pub fn installations(&self) -> Vec<(i64, String)> {
        vec![(1, "org".to_string())]
    }

    /// GitHub's time by the fake's clock, in seconds since 1970: what the tokens it makes expire by.
    pub fn now_epoch(&self) -> i64 {
        self.lock().now()
    }

    /// An installation token that only reads, made as the console makes one with its own App's key (no request
    /// reaches the fake): it reads every repository the fake has, and GitHub refuses it every write. With when it
    /// expires.
    pub fn mint_read_token(&self) -> (String, i64) {
        let mut st = self.lock();
        let token = st.token("ghs");
        let exp = st.now() + st.opts.installation_token_ttl.as_secs() as i64;
        st.installation_tokens.insert(token.clone(), exp);
        st.read_only.insert(token.clone());
        (token, exp)
    }

    /// A token that registers Actions runners with `repo`, made as the console makes one, with when it expires; none
    /// for a repository the App lacks.
    pub fn mint_registration_token(&self, repo: &str) -> Option<(String, i64)> {
        self.lock().registration_token(repo)
    }

    /// An Actions runner for one job of `repo`, registered as the console registers one: its ID and configuration,
    /// or GitHub's status (404 for a repository the App lacks, 409 for a name taken, 422 for no labels).
    pub fn mint_plain_jit(&self, repo: &str, name: &str, labels: &[String]) -> Result<(i64, String), u16> {
        let mut st = self.lock();
        let body = json!({"name": name, "runner_group_id": 1, "labels": labels, "work_folder": "_work"});
        match plain_jit(&mut st, repo, &body) {
            Ok(_) => {
                let id = st
                    .runners
                    .iter()
                    .find(|r| r.name == name)
                    .map(|r| r.id)
                    .unwrap_or(0);
                Ok((id, jit_config_of(name)))
            }
            Err(Reply::Response { status, .. }) => Err(status),
            Err(_) => Err(500),
        }
    }

    /// This installation token stops working (`DELETE /installation/token`): GitHub answers 401 to it.
    pub fn revoke_token(&self, token: &str) {
        let mut st = self.lock();
        st.installation_tokens.remove(token);
        st.read_only.remove(token);
    }

    /// Whether GitHub still takes this installation token: made here, not revoked and not expired.
    pub fn token_works(&self, token: &str) -> bool {
        let st = self.lock();
        st.installation_tokens
            .get(token)
            .is_some_and(|exp| *exp > st.now())
    }

    /// Every installation token stops working: GitHub answers 401 to each.
    pub fn revoke_installation_tokens(&self) {
        let mut st = self.lock();
        st.installation_tokens.clear();
        st.read_only.clear();
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

    /// Answers every request whose `METHOD path` contains `matcher` with `reply` from now on, for as long as GitHub
    /// is in that state, until [`FakeGithub::recover`].
    pub fn fail_until_recovered(&self, matcher: &str, reply: Reply) {
        self.fail(matcher, reply, usize::MAX);
    }

    /// Ends every fault set for requests whose `METHOD path` contains `matcher`.
    pub fn recover(&self, matcher: &str) {
        self.lock().faults.retain(|fault| fault.matcher != matcher);
    }

    /// Handles the next `times` requests whose `METHOD path` contains `matcher` and then drops the connection, so
    /// the caller never sees the answer to what was done.
    pub fn lose_answer(&self, matcher: &str, times: usize) {
        self.lock().lost.push((matcher.to_string(), times));
    }

    /// Handles the next `times` requests whose `METHOD path` contains `matcher` only after `wait`: GitHub is slow
    /// to do them, and does them all the same.
    pub fn delay(&self, matcher: &str, wait: Duration, times: usize) {
        self.lock().delays.push((matcher.to_string(), wait, times));
    }

    /// Makes Actions runner registration name `url` as the Actions service from now on.
    pub fn send_the_service_to(&self, url: &str) {
        self.lock().service_at = Some(url.to_string());
    }

    /// Makes sessions, new and refreshed, name `url` as their message queue from now on.
    pub fn send_the_queue_to(&self, url: &str) {
        self.lock().queue_at = Some(url.to_string());
    }

    /// Forgets every admin token: the service refuses them although they have not run out.
    pub fn revoke_admin_tokens(&self) {
        self.lock().admin_tokens.clear();
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

    /// Forgets a scale set's session on the service, with its pending messages: the next poll finds none.
    pub fn lose_session(&self, scale_set: i64) {
        if let Some(s) = self.lock().scale_set(scale_set) {
            s.session = None;
            s.queue.clear();
        }
        self.inner.notify.notify_waiters();
    }

    // ---- jobs ----

    /// Queues a job of the main branch asking for these labels; every matching scale set is offered it. Returns its
    /// request ID.
    pub fn queue_job(&self, repo: &str, labels: &[&str], display_name: &str) -> i64 {
        self.queue_job_at(repo, labels, display_name, "refs/heads/main")
    }

    /// [`FakeGithub::queue_job`] for a job whose workflow runs at `git_ref`: a `refs/pull/...` ref is a pull request's
    /// (event `pull_request`), any other a push's.
    pub fn queue_job_at(&self, repo: &str, labels: &[&str], display_name: &str, git_ref: &str) -> i64 {
        self.queue_run(repo, &[(labels, display_name)], git_ref)[0]
    }

    /// Queues the jobs of one workflow run at `git_ref`, each with its labels and name, as [`FakeGithub::queue_job`]
    /// does. Returns their request IDs, which are also the IDs the REST view of the run gives its jobs.
    pub fn queue_run(&self, repo: &str, jobs: &[(&[&str], &str)], git_ref: &str) -> Vec<i64> {
        let mut st = self.lock();
        let run_id = 7000 + st.next_job + 1;
        let mut ids = Vec::new();
        for (labels, display_name) in jobs {
            st.next_job += 1;
            let job = Job {
                request_id: st.next_job,
                repo: repo.to_string(),
                labels: labels.iter().map(|l| l.to_lowercase()).collect(),
                written: labels.iter().map(|l| l.to_string()).collect(),
                created_at: st.now(),
                display_name: display_name.to_string(),
                git_ref: git_ref.to_string(),
                run_id,
                state: JobState::Available,
                assigned_at: 0,
            };
            ids.push(job.request_id);
            let sets = if st.opts.auto_assign {
                vec![]
            } else {
                st.eligible_sets(&job)
            };
            for set in sets {
                let msg = st.job_message(
                    "JobAvailable",
                    &job,
                    json!({"acquireJobUrl": "https://unused.invalid"}),
                );
                st.push(set, vec![msg]);
            }
            st.jobs.push(job);
        }
        look_at_the_queue(&mut st);
        drop(st);
        self.inner.notify.notify_waiters();
        ids
    }

    /// Ends a job as its run shows it, whoever ran it: an Actions runner elsewhere, or a cancel before any took it.
    pub fn end_job(&self, request_id: i64, result: &str) {
        let mut st = self.lock();
        if let Some(job) = st.jobs.iter_mut().find(|j| j.request_id == request_id) {
            job.state = JobState::Completed(result.to_string());
        }
        look_at_the_queue(&mut st);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// GitHub looks at its queue by itself, as it does at times nobody can predict: every scale set with a session
    /// gets the queued jobs its polls have room for.
    pub fn github_looks(&self) {
        let mut st = self.lock();
        take_back(&mut st);
        look_at_the_queue(&mut st);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// The Actions runner of this name, registered outside any scale set, comes online: GitHub looks at its queue,
    /// as it did within seconds in probe N, and may hand a job to a scale set rather than to it. An Actions runner
    /// of a scale set, or one not registered, changes nothing (probes J1 to J3, L).
    pub fn runner_online(&self, runner_name: &str) {
        let mut st = self.lock();
        if !st
            .runners
            .iter()
            .any(|r| r.name == runner_name && r.scale_set == PLAIN)
        {
            return;
        }
        take_back(&mut st);
        look_at_the_queue(&mut st);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// Another Actions runner takes the job before this one's scale sets do.
    pub fn take_job(&self, request_id: i64) {
        let mut st = self.lock();
        if let Some(j) = st.jobs.iter_mut().find(|j| j.request_id == request_id) {
            j.state = JobState::TakenElsewhere;
        }
        look_at_the_queue(&mut st);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// The Actions runner with this name takes its job and starts it: one of a scale set a job assigned to the scale
    /// set, one registered outside any the oldest queued job of its repository whose labels it has. Nothing happens
    /// when there is none.
    pub fn start_job(&self, runner_name: &str) {
        let mut st = self.lock();
        if take_back(&mut st) {
            look_at_the_queue(&mut st);
        }
        let Some(runner) = st.runners.iter().find(|r| r.name == runner_name).cloned() else {
            return;
        };
        let Some(i) = st.jobs.iter().position(|j| {
            if runner.scale_set == PLAIN {
                j.state == JobState::Available && j.repo == runner.repo && matches(&runner.labels, &j.labels)
            } else {
                j.state == JobState::Acquired(runner.scale_set)
            }
        }) else {
            return;
        };
        st.jobs[i].state = JobState::Running(runner.name.clone());
        let job = st.jobs[i].clone();
        if runner.scale_set == PLAIN {
            // A job taken from the queue, as by an Actions runner elsewhere.
            look_at_the_queue(&mut st);
        } else {
            let msg = st.job_message(
                "JobStarted",
                &job,
                json!({"runnerId": runner.id, "runnerName": runner.name}),
            );
            st.push(runner.scale_set, vec![msg]);
        }
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// The job of the Actions runner with this name completes with a result (`succeeded`, `failed`, ...). A one-job
    /// Actions runner unregisters itself, as GitHub's does.
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
        look_at_the_queue(&mut st);
        drop(st);
        self.inner.notify.notify_waiters();
    }

    /// The job is cancelled. One a scale set had acquired completes as `canceled` with no Actions runner.
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
        look_at_the_queue(&mut st);
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

    /// The names of the Actions runners currently registered.
    pub fn runners(&self) -> Vec<String> {
        self.lock().runners.iter().map(|r| r.name.clone()).collect()
    }

    /// The Actions runners currently registered outside any scale set, each with its repository and labels.
    pub fn plain_runners(&self) -> Vec<(String, String, Vec<String>)> {
        self.lock()
            .runners
            .iter()
            .filter(|r| r.scale_set == PLAIN)
            .map(|r| (r.name.clone(), r.repo.clone(), r.labels.clone()))
            .collect()
    }

    /// The Actions runners whose registration a DELETE removed (answered 204), not their own end after a job.
    pub fn removed_at_request(&self) -> Vec<String> {
        self.lock().removed_at_request.clone()
    }

    /// Whether a scale set of `repo` with a session offered room beyond the jobs it holds in both of its last two
    /// polls: a job of that repository queued now goes to it at once. Another repository's room is no room for it.
    pub fn offers_room(&self, repo: &str) -> bool {
        self.lock().offers_room(repo)
    }

    /// Whether this job waits in the queue while a scale set of its repository offers room
    /// ([`FakeGithub::offers_room`]): GitHub's next look hands it out.
    pub fn offers_room_for(&self, request_id: i64) -> bool {
        let st = self.lock();
        st.jobs
            .iter()
            .find(|j| j.request_id == request_id)
            .is_some_and(|j| j.state == JobState::Available && st.offers_room(&j.repo))
    }

    /// How often a job was handed to a scale set: once per job, and once more for each take-back that was handed out
    /// again.
    pub fn assignments(&self) -> usize {
        self.lock().assignments
    }

    /// A job's state.
    pub fn job_state(&self, request_id: i64) -> Option<JobState> {
        self.lock()
            .jobs
            .iter()
            .find(|j| j.request_id == request_id)
            .map(|j| j.state.clone())
    }

    /// The run of this job is a pull request's from a fork: its head repository is another one.
    pub fn from_fork(&self, request_id: i64) {
        let mut st = self.lock();
        if let Some(run) = st
            .jobs
            .iter()
            .find(|j| j.request_id == request_id)
            .map(|j| j.run_id)
        {
            st.forks.insert(run);
        }
    }

    /// How many conditional requests were answered 304 Not Modified so far.
    pub fn not_modified(&self) -> usize {
        self.lock().not_modified
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
        if self.lock().down {
            return Reply::Drop;
        }
        let line = req.line();
        let hook = {
            let mut st = self.lock();
            st.log.push(Logged {
                line: line.clone(),
                query: req.query.clone(),
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
        let delay = {
            let mut st = self.lock();
            st.delays
                .iter_mut()
                .find(|(m, _, left)| *left > 0 && line.contains(m))
                .map(|delay| {
                    delay.2 -= 1;
                    delay.1
                })
        };
        if let Some(wait) = delay {
            tokio::time::sleep(wait).await;
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
        {
            let mut st = self.lock();
            if let Some(lost) = st.lost.iter_mut().find(|(m, left)| *left > 0 && line.contains(m)) {
                lost.1 -= 1;
                return Reply::Drop;
            }
        }
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
                            .map(|(file, content)| json!({"name": file, "type": "file", "sha": sha_of(content), "size": content.len()}))
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
            // The REST view of workflow runs and their jobs, newest first, one page; each answer carries an ETag, and a
            // request that names it in If-None-Match is answered 304 with no body, as GitHub does.
            ("GET", ["repos", owner, repo, "actions", "runs"]) => {
                let name = format!("{owner}/{repo}");
                let status = req.query("status").map(String::from);
                let per_page = page_size(&req);
                self.with_installation(&req, |st| {
                    if !st.repos.contains_key(&name) {
                        return Reply::status_json(404, &json!({"message": "Not Found"}));
                    }
                    let mut runs: Vec<(i64, &'static str, i64, i64)> = Vec::new();
                    for job in st.jobs.iter().filter(|j| j.repo == name) {
                        if runs.iter().all(|(id, ..)| *id != job.run_id) {
                            let updated = st.run_updated(job.run_id, job.created_at);
                            runs.push((job.run_id, st.run_status(job.run_id), job.created_at, updated));
                        }
                    }
                    runs.retain(|(_, s, ..)| status.as_deref().is_none_or(|wanted| wanted == *s));
                    runs.sort_by_key(|run| std::cmp::Reverse(run.0));
                    let total = runs.len();
                    let page: Vec<Value> = runs
                        .into_iter()
                        .take(per_page)
                        .map(|(id, status, created, updated)| {
                            let head = if st.forks.contains(&id) { "stranger/fork".to_string() } else { name.clone() };
                            json!({"id": id, "status": status, "created_at": rfc3339(created),
                                   "updated_at": rfc3339(updated), "name": "CI",
                                   "head_repository": {"full_name": head}})
                        })
                        .collect();
                    st.conditional(&req, &json!({"total_count": total, "workflow_runs": page}))
                })
            }
            ("GET", ["repos", owner, repo, "actions", "runs", run, "jobs"]) => {
                let name = format!("{owner}/{repo}");
                let run: i64 = run.parse().unwrap_or(0);
                let per_page = page_size(&req);
                self.with_installation(&req, |st| {
                    let jobs: Vec<Value> = st
                        .jobs
                        .iter()
                        .filter(|j| j.repo == name && j.run_id == run)
                        .map(|j| {
                            json!({"id": j.request_id, "run_id": j.run_id, "name": j.display_name,
                                   "status": job_status(&j.state), "labels": j.written,
                                   "created_at": rfc3339(j.created_at)})
                        })
                        .collect();
                    if jobs.is_empty() {
                        return Reply::status_json(404, &json!({"message": "Not Found"}));
                    }
                    let total = jobs.len();
                    let page: Vec<Value> = jobs.into_iter().take(per_page).collect();
                    st.conditional(&req, &json!({"total_count": total, "jobs": page}))
                })
            }
            ("POST", ["repos", owner, repo, "actions", "runners", "registration-token"]) => {
                let name = format!("{owner}/{repo}");
                let writes = writes(&req);
                self.with_installation(&req, |st| {
                    if st.read_only.contains(&writes) {
                        return not_accessible();
                    }
                    match st.registration_token(&name) {
                        Some((token, exp)) => {
                            Reply::status_json(201, &json!({"token": token, "expires_at": rfc3339(exp)}))
                        }
                        None => Reply::status_json(404, &json!({"message": "Not Found"})),
                    }
                })
            }
            // Probe H, 2026-10-12: `busy` and `status` for an Actions runner registered for a scale set, by the ID its
            // registration gave; 404 once it is removed.
            ("GET", ["repos", owner, repo, "actions", "runners", id]) => {
                let name = format!("{owner}/{repo}");
                let id: i64 = id.parse().unwrap_or(0);
                self.with_installation(&req, |st| {
                    let Some(runner) = st.registered.get(&id).filter(|r| r.repo == name).cloned()
                    else {
                        return Reply::status_json(404, &json!({"message": "Not Found"}));
                    };
                    let now = st.runners.iter().any(|r| r.id == id).then(|| {
                        st.jobs
                            .iter()
                            .any(|j| matches!(&j.state, JobState::Running(n) if *n == runner.name))
                    });
                    let before = st.rest_seen.insert(id, now).unwrap_or(Some(false));
                    match if st.rest_stale { before } else { now } {
                        Some(busy) => Reply::json(
                            &json!({"id": runner.id, "name": runner.name, "status": "online", "busy": busy}),
                        ),
                        None => Reply::status_json(404, &json!({"message": "Not Found"})),
                    }
                })
            }
            // Probe M, 2026-10-17: an Actions runner for one job outside any scale set, with exactly the labels asked
            // for, in the repository's default runner group; a name taken is refused.
            ("POST", ["repos", owner, repo, "actions", "runners", "generate-jitconfig"]) => {
                let name = format!("{owner}/{repo}");
                let writes = writes(&req);
                self.with_installation(&req, |st| {
                    if st.read_only.contains(&writes) {
                        return not_accessible();
                    }
                    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
                    match plain_jit(st, &name, &body) {
                        Ok(reply) | Err(reply) => reply,
                    }
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
        let url = st
            .service_at
            .clone()
            .unwrap_or_else(|| format!("{}/svc", st.base));
        Reply::json(&json!({"url": url, "token": admin}))
    }

    async fn actions_service(&self, req: &Request, segs: &[&str]) -> Reply {
        // Message queue calls carry the session token; everything else the admin token.
        let bearer = req
            .header("authorization")
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or("")
            .to_string();
        let m = req.method.as_str();
        // /svc/_apis/runtime/runnerscalesets[/id[/sessions[/sid] | /acquirejobs | /generatejitconfig |
        // /messages[/mid]]]
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
                            s.queue.clear();
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
        if let ("GET", ["_apis", "distributedtask", "pools", "0", "agents"]) = (m, segs) {
            let name = req.query("agentName").unwrap_or("").to_string();
            return self.admin(&bearer, |st, _| {
                let found: Vec<Value> = st
                    .runners
                    .iter()
                    .filter(|r| r.name == name)
                    .map(|r| json!({"id": r.id, "name": r.name}))
                    .collect();
                Reply::json(&json!({"count": found.len(), "value": found}))
            });
        }
        if let ("DELETE", ["_apis", "distributedtask", "pools", "0", "agents", id]) = (m, segs) {
            let id: i64 = id.parse().unwrap_or(0);
            return self.admin(&bearer, |st, _| {
                let Some(name) = st.runners.iter().find(|r| r.id == id).map(|r| r.name.clone()) else {
                    return json_error(404, "AgentNotFoundException", "runner not found");
                };
                // As the service: an Actions runner is not removed under its job, whatever became of its machine.
                if st
                    .jobs
                    .iter()
                    .any(|j| matches!(&j.state, JobState::Running(n) if *n == name))
                {
                    return json_error(422, "JobStillRunningException", "the runner still has a job");
                }
                st.runners.retain(|r| r.id != id);
                st.removed_at_request.push(name);
                Reply::empty(204)
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
        let mine = {
            let mut st = self.lock();
            let authorized = st
                .scale_set(id)
                .and_then(|set| set.session.as_ref())
                .is_some_and(|s| s.token == bearer);
            st.scale_set(id).map(|set| {
                set.polls += 1;
                if authorized {
                    set.previous_capacity = std::mem::replace(&mut set.capacity, capacity);
                }
                set.polls
            })
        };
        loop {
            let notified = self.inner.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let answer = self.session_auth(id, bearer, |st| {
                if st.scale_set(id).map(|set| set.polls) != mine {
                    return Reply::empty(202);
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

/// How long GitHub leaves a job with a scale set whose Actions runner does not take it. Seen: probe J2 and the Mac
/// mini at 18:57:52 (`docs/github-protocol.md`).
pub const TAKE_BACK_AFTER: Duration = Duration::from_secs(5 * 60);

/// Takes back every assignment no Actions runner took within [`TAKE_BACK_AFTER`]: the scale set hears a
/// `JobCompleted` `canceled` with no Actions runner, and the job waits in the queue again for GitHub's next look
/// (inferred: in J2 it was assigned again at once with the job queued next). Says whether one was taken back; the
/// caller looks at the queue then, as a take-back is a queue event.
fn take_back(st: &mut State) -> bool {
    let now = st.queue_now();
    let after = i64::try_from(TAKE_BACK_AFTER.as_secs()).unwrap_or(i64::MAX);
    let mut took = false;
    for i in 0..st.jobs.len() {
        let JobState::Acquired(set) = st.jobs[i].state else {
            continue;
        };
        if now - st.jobs[i].assigned_at < after {
            continue;
        }
        took = true;
        st.jobs[i].state = JobState::Available;
        let job = st.jobs[i].clone();
        let msg = st.job_message("JobCompleted", &job, json!({"result": "canceled"}));
        st.push(set, vec![msg]);
    }
    took
}

/// What the service does when something happens in its queue: every scale set with a session gets the queued jobs
/// its newest poll has room for.
fn look_at_the_queue(st: &mut State) {
    let ids: Vec<i64> = st
        .scale_sets
        .iter()
        .filter(|s| !s.deleted && s.session.is_some())
        .map(|s| s.id)
        .collect();
    for id in ids {
        assign(st, id);
    }
}

/// Assigns queued jobs to the scale set up to the capacity of its newest poll: a `JobAssigned` for each.
fn assign(st: &mut State, id: i64) {
    if !st.opts.auto_assign {
        return;
    }
    let Some(set) = st.scale_sets.iter().find(|s| s.id == id && !s.deleted) else {
        return;
    };
    let (labels, repo) = (set.labels.clone(), set.repo.clone());
    let (Some(newest), Some(before)) = (set.capacity, set.previous_capacity) else {
        return;
    };
    if set.session.is_none() {
        return;
    }
    let capacity = newest.min(before);
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
        st.jobs[i].assigned_at = st.queue_now();
        st.assignments += 1;
        let job = st.jobs[i].clone();
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
        polls: 0,
        capacity: None,
        previous_capacity: None,
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

/// A session as the service sends it. Only an opening brings the counts.
fn session_json(st: &State, set_id: i64, s: &Session, opening: bool) -> Value {
    json!({
        "sessionId": s.id, "ownerName": s.owner,
        "messageQueueUrl": st.queue_at.clone().unwrap_or_else(|| format!("{}/svc/_apis/runtime/runnerscalesets/{}/messages", st.base, set_id)),
        "messageQueueAccessToken": s.token,
        "statistics": if opening { st.stats_json(set_id) } else { Value::Null },
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
        expired: false,
        expires_at: now + ttl,
    };
    set.session = Some(session.clone());
    set.capacity = None;
    set.previous_capacity = None;
    Reply::json(&session_json(st, id, &session, true))
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
            s.expired = false;
            s.expires_at = now + ttl;
            let s = s.clone();
            Reply::json(&session_json(st, id, &s, false))
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
            st.jobs[i].assigned_at = st.queue_now();
            st.assignments += 1;
            got.push(rid);
            let job = st.jobs[i].clone();
            let msg = st.job_message("JobAssigned", &job, json!({}));
            st.push(id, vec![msg]);
        }
    }
    Reply::json(&json!({"count": got.len(), "value": got}))
}

/// The bearer token of a request, for the check that it may write.
fn writes(req: &Request) -> String {
    req.header("authorization")
        .and_then(|h| h.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_string()
}

/// GitHub's answer to a write with a token that may not make it.
fn not_accessible() -> Reply {
    Reply::status_json(403, &json!({"message": "Resource not accessible by integration"}))
}

/// `generate-jitconfig` with the request's JSON body: the 201, or GitHub's refusal.
fn plain_jit(st: &mut State, repo: &str, body: &Value) -> Result<Reply, Reply> {
    if !st.repos.contains_key(repo) {
        return Err(Reply::status_json(404, &json!({"message": "Not Found"})));
    }
    let name = body["name"].as_str().unwrap_or_default().to_string();
    let labels: Vec<String> = body["labels"]
        .as_array()
        .map(|labels| {
            labels
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_lowercase)
                .collect()
        })
        .unwrap_or_default();
    if name.is_empty() || labels.is_empty() || body["runner_group_id"].as_i64().is_none() {
        return Err(Reply::status_json(
            422,
            &json!({"message": "Invalid request: name, runner_group_id and labels are required"}),
        ));
    }
    let Some(rid) = register(st, &name, PLAIN, repo.to_string(), labels.clone()) else {
        return Err(Reply::status_json(
            409,
            &json!({"message": "Already exists - A runner with this name already exists."}),
        ));
    };
    let labels: Vec<Value> = labels
        .iter()
        .map(|l| json!({"name": l, "type": "custom"}))
        .collect();
    Ok(Reply::status_json(
        201,
        &json!({
            "runner": {"id": rid, "name": name, "os": "unknown", "status": "offline", "busy": false, "labels": labels},
            "encoded_jit_config": jit_config_of(&name),
        }),
    ))
}

/// The configuration the fake gives an Actions runner registered outside any scale set.
fn jit_config_of(name: &str) -> String {
    b64url(format!("jit-config-for-{name}").as_bytes())
}

/// Registers an Actions runner and says its ID; none when the name is taken.
fn register(st: &mut State, name: &str, scale_set: i64, repo: String, labels: Vec<String>) -> Option<i64> {
    if st.runners.iter().any(|r| r.name == name) {
        return None;
    }
    st.next_runner += 1;
    let rid = st.next_runner;
    let runner = Runner {
        id: rid,
        name: name.to_string(),
        scale_set,
        repo,
        labels,
    };
    st.registered.insert(rid, runner.clone());
    st.runners.push(runner);
    Some(rid)
}

fn generate_jit(st: &mut State, id: i64, req: &Request) -> Reply {
    let body: Value = serde_json::from_slice(&req.body).unwrap_or_default();
    let name = body["name"].as_str().unwrap_or_default().to_string();
    if name.is_empty() {
        return json_error(400, "InvalidRunnerException", "name required");
    }
    let Some(repo) = st.scale_set(id).map(|set| set.repo.clone()) else {
        return json_error(404, "RunnerScaleSetNotFoundException", "scale set not found");
    };
    let Some(rid) = register(st, &name, id, repo, Vec::new()) else {
        return json_error(409, "AgentExistsException", "a runner with this name exists");
    };
    Reply::json(&json!({
        "runner": {"id": rid, "name": name, "runnerScaleSetId": id},
        "encodedJITConfig": b64url(format!("jit-config-for-{name}").as_bytes()),
    }))
}

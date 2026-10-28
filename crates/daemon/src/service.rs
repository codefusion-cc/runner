//! The Runner's service: it finds the repositories and sizes to serve, keeps one listener per scale set, lets the
//! machines see the time pass, removes the registrations at GitHub that have to go, and reports to the console.
//!
//! Every round is a method of its own ([`Service::discover`], [`Service::sweep`], [`Service::report`]) so that a
//! test drives them one at a time; [`Service::run`] is the loop that calls them until it is told to stop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use cf_budget::{Battery, Host, Pressure, Size};
use cf_console_client::{
    Activity, Answer, Client, Command, CommandResult, DockerStatus, HeldJobs, HostStatus, JobKind, JobLimits,
    MAX_RUNNERS, MAX_SCALE_SETS, MachineInfo, Os, Outbox, Power, Report, RunnerJob, RunnerStatus,
    RunnerUsage, ScaleSetStatus, Share, SizeStatus, Stalls, Status, Update,
};
use cf_ghapp::{Clock, GitHubApp, Repo};
use cf_scaleset::{Backoff, ScaleSetClient, Shutdown, ShutdownTrigger, Statistics, listen_nudged, supervise};
use cf_workflows::{LabelSet, self_hosted_with};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::adapters::SetRegistry;
use crate::held::{self, HeldSet, OwnRunners, SetView};
use crate::machines::{self, JobReport, Machines, Reports, Snapshot};
use crate::names::{ServePolicy, scale_set_name};
use crate::queued::{Bounds, Look, Queued, QueuedSet};
use crate::roster::{Roster, Runner};
use crate::scaler::{Scaler, ScalerConfig, Start, reconcile, start_runner};
use crate::secs;
use crate::test_run::{self, Admission, TestRun, Waited};
use crate::traits::{Claim, EventKind, Hosts, Slots, Ticket};
use crate::update::{Pending, SelfUpdate, SettingsChange, SettingsFile, SettingsLook, commit_from_args};
use crate::vms::Vms;

/// Wakes every listener, so that each asks its scaler again: a slot opened, or a job that waited may start.
#[derive(Debug, Default)]
pub struct Nudge(Mutex<Vec<Weak<Notify>>>);

impl Nudge {
    /// A listener's own bell. One ring is kept for a listener that is busy when it comes.
    pub fn subscribe(&self) -> Arc<Notify> {
        let bell = Arc::new(Notify::new());
        let mut bells = self.0.lock().expect("nudge");
        bells.retain(|bell| bell.strong_count() > 0);
        bells.push(Arc::downgrade(&bell));
        bell
    }

    /// Rings every bell.
    pub fn fire(&self) {
        for bell in self.0.lock().expect("nudge").iter().filter_map(Weak::upgrade) {
            bell.notify_one();
        }
    }
}

/// How the service runs on one Mac.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    /// This Mac's name: the start of every Actions runner's and scale set's name.
    pub machine: String,
    /// The Mac's host name, for the console.
    pub hostname: String,
    /// The Mac's model, for the console.
    pub model: String,
    /// The Runner's version.
    pub version: String,
    /// The system the Runner runs on, for the console.
    pub os: Option<Os>,
    /// Which repositories are served.
    pub policy: ServePolicy,
    /// How long one long poll may take in all.
    pub poll_timeout: Duration,
    /// How often repositories and workflows are read again.
    pub discover_every: Duration,
    /// How often registrations that have to go are removed.
    pub sweep_every: Duration,
    /// How long a registration GitHub would not remove is left alone.
    pub sweep_again: Duration,
    /// How often the machines look at the Mac.
    pub tick_every: Duration,
    /// How often the console hears from the Mac when it names no other time.
    pub report_every: Duration,
    /// The largest workflow file that is read.
    pub max_workflow_bytes: u64,
    /// The first and the longest wait after a listener failed, and how long one must work for the wait to start
    /// small again.
    pub backoff: (Duration, Duration, Duration),
    /// The longest an update may take from the console's command to the install, the wait for the machines that
    /// stand included. Under the hour the console waits for a command's result. New settings wait as long for the
    /// machines that stand.
    pub update_limit: Duration,
    /// How often the settings file is looked at.
    pub settings_every: Duration,
    /// How often the jobs GitHub has queued are read, for the label sets the default branch's workflows do not
    /// name: a pull request's new workflow or labels.
    pub queue_every: Duration,
    /// How long one read of every repository's queue may take in all; a repository not read in time keeps what it
    /// has.
    pub queue_budget: Duration,
    /// How long a label set only queued jobs ask for is served after a job of it was last seen queued or running.
    pub queued_keep: Duration,
    /// The most label sets of queued jobs served for one repository, and for the host: the newest are.
    pub queued_bounds: (usize, usize),
    /// Where a GitHub App key this host held before the console held it would be (`<the old app_dir>/app.pem`), as
    /// the update that deleted it remembered: reported to the console while a file is there (`appKeyFile`).
    pub app_key_file: Option<PathBuf>,
}

impl ServiceConfig {
    /// The times the Runner keeps in service.
    pub fn new(machine: &str, hostname: &str, model: &str, version: &str) -> ServiceConfig {
        ServiceConfig {
            machine: machine.to_string(),
            hostname: hostname.to_string(),
            model: model.to_string(),
            version: version.to_string(),
            os: Os::this(),
            policy: ServePolicy::default(),
            poll_timeout: Duration::from_secs(70),
            discover_every: Duration::from_secs(300),
            sweep_every: Duration::from_secs(5),
            sweep_again: Duration::from_secs(30),
            tick_every: Duration::from_secs(1),
            report_every: Duration::from_secs(60),
            max_workflow_bytes: 1 << 20,
            backoff: (
                Duration::from_secs(5),
                Duration::from_secs(300),
                Duration::from_secs(300),
            ),
            update_limit: Duration::from_secs(50 * 60),
            settings_every: Duration::from_secs(30),
            queue_every: Duration::from_secs(60),
            queue_budget: Duration::from_secs(45),
            queued_keep: Duration::from_secs(30 * 60),
            queued_bounds: (8, 32),
            app_key_file: None,
        }
    }
}

/// The console and the job events that wait for it.
pub struct Console {
    /// The client, with this Mac's token.
    pub client: Client,
    /// The job events that wait.
    pub outbox: Arc<Outbox>,
}

/// What the Mac reads beside the budget's reading, for the console's page.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostExtras {
    /// The load averages of 1, 5 and 15 minutes.
    pub load: [f64; 3],
    /// The size of the disk the machines' files are on.
    pub disk_total_bytes: u64,
    /// The swap space the system has made.
    pub swap_total_bytes: u64,
    /// The CPU in use, in percent; -1 when it is not known.
    pub cpu_percent: f64,
    /// When the Mac started, in seconds since 1970, when it is known.
    pub boot: Option<i64>,
    /// The battery, on a Mac that has one.
    pub battery: Option<Battery>,
}

/// One scale set the Mac serves, as a report shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetState {
    /// `owner/name`.
    pub repo: String,
    /// The scale set's name.
    pub name: String,
    /// GitHub's number for it; 0 until GitHub made it.
    pub id: i64,
    /// Its labels.
    pub labels: Vec<String>,
    /// The size of its machines.
    pub size: Size,
    /// Whether its jobs get a Docker daemon (`cf-docker`).
    pub docker: bool,
    /// Why its listener is not listening, when it is not.
    pub error: Option<String>,
    /// Since when, in seconds since 1970.
    pub error_at: Option<i64>,
    /// GitHub's counts, as the listener last heard them.
    pub statistics: Option<Statistics>,
    /// Why a job assigned to it can never start on this Mac, when one cannot.
    pub never: Option<String>,
    /// Since when every poll of its listener offered GitHub room beyond the jobs GitHub counts as assigned to it;
    /// none while the last did not, or none listens.
    pub room_since: Option<SystemTime>,
}

/// One look's reading of GitHub.
struct Read {
    /// The scale sets to serve, by name.
    wanted: BTreeMap<String, Wanted>,
    /// Repositories that could not be read: nothing is concluded about them.
    unread: BTreeSet<String>,
    problems: Vec<String>,
    /// The repositories this Mac serves, with the installation that gives each.
    repos: Vec<(Repo, i64)>,
}

/// One look's reading of the jobs GitHub has queued: the sets this host would serve for them that main does not
/// name, the repositories whose queue could not be read, and what went wrong.
#[derive(Default)]
struct QueueRead {
    seen: Vec<(String, QueuedSet)>,
    queued: QueuedNow,
    unread: BTreeSet<String>,
    problems: Vec<String>,
}

/// The jobs the REST view calls queued in the repositories one look read.
#[derive(Debug, Default)]
struct QueuedNow {
    /// By the scale set this host would serve them with; a set none of whose jobs is queued is not named.
    jobs: BTreeMap<String, QueuedJobs>,
    /// The repositories whose queue was read.
    read: BTreeSet<String>,
}

/// The queued jobs of one label set of a repository.
#[derive(Debug, Default)]
struct QueuedJobs {
    repo: String,
    labels: Vec<String>,
    /// When each was queued and its ID, the oldest first.
    jobs: Vec<(i64, i64)>,
}

/// The jobs GitHub holds back from scale sets with room, as the last look at the queue found them, and the Actions
/// runners of their own the scale sets got ([`crate::held`]).
#[derive(Debug, Default)]
struct Held {
    sets: Vec<HeldSet>,
    own: OwnRunners,
}

#[derive(Clone)]
struct Wanted {
    repo: String,
    client: ScaleSetClient,
    labels: Vec<String>,
    size: Size,
    docker: bool,
}

struct Serving {
    stop: ShutdownTrigger,
    task: JoinHandle<()>,
    state: Arc<Mutex<SetState>>,
}

/// Everything the service is made of.
pub struct ServiceParts<H, V> {
    /// The GitHub App.
    pub app: GitHubApp,
    /// The machines. Their `nudge` rings [`ServiceParts::nudge`].
    pub machines: Arc<Machines<H, V>>,
    /// The Actions runners, shared with the machines.
    pub roster: Arc<Roster>,
    /// The wall clock.
    pub clock: Arc<dyn Clock>,
    /// The bell the machines ring.
    pub nudge: Arc<Nudge>,
    /// The console; none on a Mac that is not enrolled.
    pub console: Option<Console>,
    /// How the service runs.
    pub config: ServiceConfig,
    /// Reads what the console's page shows beside the budget's reading.
    pub extras: Box<dyn Fn() -> HostExtras + Send + Sync>,
    /// How the Runner updates itself; none where it cannot.
    pub updates: Option<Arc<dyn SelfUpdate>>,
    /// Where what came of the console's commands is kept until the console has it, so that a Runner that starts
    /// again still says it, and does not do a command twice; none to keep it in memory only.
    pub results: Option<PathBuf>,
    /// The settings file the Runner started with, whose changes it takes up by starting again; none to take up
    /// none.
    pub settings: Option<Arc<dyn SettingsFile>>,
}

struct Shared<H, V> {
    app: GitHubApp,
    machines: Arc<Machines<H, V>>,
    roster: Arc<Roster>,
    clock: Arc<dyn Clock>,
    nudge: Arc<Nudge>,
    console: Option<Console>,
    config: ServiceConfig,
    extras: Box<dyn Fn() -> HostExtras + Send + Sync>,
    serving: Mutex<BTreeMap<String, Serving>>,
    /// A scale set client per repository that was ever served, for removing what is left there.
    clients: Mutex<BTreeMap<String, ScaleSetClient>>,
    discovery_error: Mutex<Option<String>>,
    not_serving: Mutex<Option<String>>,
    /// The last look at the default branches, which a look at the queue alone builds on; none before the first
    /// and once the Mac wants no scale sets.
    main: Mutex<Option<Arc<Read>>>,
    /// The label sets of queued jobs that are served beside main's.
    queued: Mutex<Queued>,
    /// Where the next look at the queues starts among the repositories: the first one the last look had no time
    /// for, so that no repository is late every time.
    queue_cursor: AtomicUsize,
    results: Mutex<Vec<CommandResult>>,
    /// Registrations GitHub would not remove, and when each is tried again.
    refused: Mutex<BTreeMap<String, Instant>>,
    console_paused: Mutex<bool>,
    /// The outbox's count of queued events as of the last report, and whether the console took that report: news
    /// goes to a console that answers at once, and to one that does not with the next report in its turn.
    told: Mutex<(u64, bool)>,
    /// Jobs that wait while the Mac could run them, since the first look that saw them.
    stall: Mutex<Option<Stall>>,
    held: Mutex<Held>,
    updates: Option<Arc<dyn SelfUpdate>>,
    results_file: Option<PathBuf>,
    update: Mutex<Updating>,
    settings: Option<Arc<dyn SettingsFile>>,
    applying: Mutex<Applying>,
    /// Set when the Runner is to end so that the service manager starts it again: a restart, or an update installed.
    restart: AtomicBool,
    restarting: Notify,
    /// Set while the Runner ends: the last report says so instead of serving nothing.
    ending: AtomicBool,
    /// Set once the first look at GitHub ended, read or not: until then the reports say the Runner starts.
    looked: AtomicBool,
    /// Whether the last look at the Docker check found Docker refused: a change reads the label sets again.
    docker_refused: AtomicBool,
    /// Rung when the label sets are to be read again before the next discovery in its turn: the looks at GitHub
    /// take it one at a time with the others.
    rediscover: Notify,
    /// The command of the test run that waits for its turn, which is not done again while it waits, and whether the
    /// console still sends it.
    test_run: Mutex<Option<(String, Arc<AtomicBool>)>>,
}

/// What the console's page says of a settings file no Runner could start on.
fn unusable(why: &str) -> String {
    format!("the settings file changed and is not used: {why}")
}

/// What new settings have come to.
#[derive(Debug, Default)]
struct Applying {
    /// Whether the Runner waits for its machines to end, to start again on new settings.
    under_way: bool,
    /// The version of the file whose wait ran out: not waited for again.
    gave_up: Option<String>,
    /// Why the file is unusable, once said.
    told: Option<String>,
    /// What the console's page shows until the file changes, and since when: a file no Runner could start on, or a
    /// wait that ran out.
    warning: Option<(String, i64)>,
}

/// What the Runner's updates have come to.
#[derive(Debug, Default)]
struct Updating {
    /// The command of the update under way, which is not done again while it is.
    command: Option<String>,
    /// What it does now, for the console, while new jobs wait for it.
    doing: Option<String>,
    /// When the console last asked for one.
    asked_at: Option<i64>,
    /// The last failure, and when, while updates keep failing.
    error: Option<(String, i64)>,
    failures: u32,
}

/// Jobs this Mac serves that wait while it could run them.
#[derive(Debug)]
struct Stall {
    since: SystemTime,
    /// Whether the log warned of them.
    warned: bool,
}

/// How long jobs handed to this Mac may wait while nothing runs here before the log warns. A machine starts within
/// seconds when one fits; a minute is far past that.
pub const STALL_WARN_AFTER: Duration = Duration::from_secs(60);

/// The Runner's service on one Mac.
pub struct Service<H, V> {
    shared: Arc<Shared<H, V>>,
}

/// The longest `discoveryError`, scale set `error` and command result `message` the console reads (it takes four
/// times the bound of a text and cuts it; over that it refuses the whole report).
const STATUS_TEXT_MAX: usize = 1000;
/// The longest `hostError` the console keeps.
const HOST_ERROR_MAX: usize = 300;
/// The most labels a scale set lists to the console.
const MAX_LABELS: usize = 30;
/// How many repositories' queues are read at once.
const QUEUE_READS: usize = 4;

/// `text` cut to `max` characters, with an ellipsis where it was cut: one long error must not make the console
/// refuse the Mac's whole report.
fn cut(text: String, max: usize) -> String {
    if text.chars().count() <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('\u{2026}');
    cut
}

/// The results a Runner before kept for the console. A file that cannot be read keeps nothing: the console sends
/// its commands again until it has their results.
fn kept_results(path: Option<&std::path::Path>) -> Vec<CommandResult> {
    let Some(path) = path else {
        return Vec::new();
    };
    let read = match std::fs::read(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        read => read.map_err(|error| error.to_string()),
    };
    read.and_then(|bytes| serde_json::from_slice(&bytes).map_err(|error| error.to_string()))
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "the results kept for the console cannot be read");
            Vec::new()
        })
}

/// A moment as the console reads it.
pub fn iso(seconds: i64) -> String {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

/// A job report as the console takes it. The console's `result` is GitHub's word when there is one and the
/// Runner's own end otherwise; `end` is always the Runner's own.
pub fn job_event(report: &JobReport) -> cf_console_client::JobEvent {
    let text = |value: &str| (!value.is_empty()).then(|| value.to_string());
    let github = &report.github;
    let machine = &report.machine;
    cf_console_client::JobEvent {
        id: report.id.clone(),
        kind: match report.kind {
            EventKind::Started => JobKind::Started,
            EventKind::Completed => JobKind::Completed,
        },
        runner: report.runner.clone(),
        repo: text(&github.repo),
        job: text(&github.job),
        workflow: text(&github.workflow),
        run_id: u64::try_from(github.run_id).ok().filter(|id| *id > 0),
        event: text(&github.event),
        queued_at: github.queued_at.map(iso),
        started_at: github.started_at.map(iso),
        at: iso(report.at),
        result: report
            .end
            .map(|end| github.result.clone().unwrap_or_else(|| end.name().to_string())),
        out_of_memory: report.out_of_memory,
        end: report.end.map(|end| end.name().to_string()),
        machine: Some(MachineInfo {
            size: machine.size.clone(),
            memory_bytes: machine.memory_bytes,
            limit_bytes: machine.limit_bytes,
            cpus: machine.cpus,
            origin: machine.origin.clone(),
            boot_ms: machine.boot_ms,
            memory_peak_bytes: machine.memory_peak_bytes,
            disk_peak_bytes: machine.disk_peak_bytes,
            cpu_usec: machine.cpu_usec,
            wall_ms: machine.wall_ms,
            stalls_peak: machine.pressure_peak.map(stalls),
            host_cpu_peak: machine.host_cpu_peak.map(share),
        }),
    }
}

/// A pressure share as the console takes it.
fn share(psi: cf_guest_proto::Psi) -> Share {
    Share {
        some: psi.some,
        full: psi.full,
    }
}

/// A job's stalls as the console takes them.
fn stalls(pressure: cf_guest_proto::Pressure) -> Stalls {
    Stalls {
        memory: share(pressure.memory),
        cpu: share(pressure.cpu),
        io: share(pressure.io),
    }
}

/// Puts job reports into the console's outbox.
pub struct OutboxReports(pub Arc<Outbox>);

impl Reports for OutboxReports {
    fn job(&self, report: JobReport) {
        if !self.0.push(job_event(&report)) {
            tracing::debug!(id = %report.id, "a job report that waits or went already was not queued again");
        }
    }
}

/// Job reports on a Mac without a console: they are logged.
pub struct LoggedReports;

impl Reports for LoggedReports {
    fn job(&self, report: JobReport) {
        tracing::info!(
            id = %report.id, repo = %report.github.repo, job = %report.github.job,
            end = report.end.map_or("", crate::EndKind::name), "job"
        );
    }
}

impl<H, V> Shared<H, V> {
    /// The scale set client of `repo` (`owner/name`) through the console, made once and kept for removing what is
    /// left there; none on a Runner that is not enrolled, which has no scale sets.
    fn client_of(&self, installation: i64, repo: &str) -> Option<ScaleSetClient> {
        let console = self.console.as_ref()?;
        let client = self
            .clients
            .lock()
            .expect("clients")
            .entry(repo.to_string())
            .or_insert_with(|| {
                ScaleSetClient::new(
                    self.app.clone(),
                    console.client.clone(),
                    &self.config.machine,
                    installation,
                    repo,
                    self.config.poll_timeout,
                )
            })
            .clone();
        Some(client)
    }
}

impl<H: Host + Send + 'static, V: Vms + 'static> Service<H, V> {
    /// Another handle on this service, for a task of its own.
    fn handle(&self) -> Service<H, V> {
        Service {
            shared: self.shared.clone(),
        }
    }

    /// A service that does nothing until its rounds are called.
    pub fn new(parts: ServiceParts<H, V>) -> Service<H, V> {
        Service {
            shared: Arc::new(Shared {
                app: parts.app,
                machines: parts.machines,
                roster: parts.roster,
                clock: parts.clock,
                nudge: parts.nudge,
                console: parts.console,
                config: parts.config,
                extras: parts.extras,
                serving: Mutex::new(BTreeMap::new()),
                clients: Mutex::new(BTreeMap::new()),
                discovery_error: Mutex::new(None),
                not_serving: Mutex::new(None),
                main: Mutex::new(None),
                queued: Mutex::new(Queued::default()),
                queue_cursor: AtomicUsize::new(0),
                refused: Mutex::new(BTreeMap::new()),
                console_paused: Mutex::new(false),
                told: Mutex::new((0, false)),
                stall: Mutex::new(None),
                held: Mutex::new(Held::default()),
                updates: parts.updates,
                update: Mutex::new(Updating::default()),
                settings: parts.settings,
                applying: Mutex::new(Applying::default()),
                restart: AtomicBool::new(false),
                restarting: Notify::new(),
                ending: AtomicBool::new(false),
                looked: AtomicBool::new(false),
                test_run: Mutex::new(None),
                docker_refused: AtomicBool::new(false),
                rediscover: Notify::new(),
                results: Mutex::new(kept_results(parts.results.as_deref())),
                results_file: parts.results,
            }),
        }
    }

    /// Says what came of a command that is not one of a report's answer: an update the Runner that starts finishes.
    /// Kept until the console has it.
    pub fn said(&self, id: &str, ok: bool, message: &str) {
        // The result is in before the command is no longer under way, as [`Service::answered`] reads them.
        let mut update = self.shared.update.lock().expect("update");
        self.push_result(id.to_string(), ok, message.to_string());
        if update.command.as_deref() == Some(id) {
            update.command = None;
        }
    }

    /// Takes on an update a Runner before installed, which this one finishes: its command is not done again
    /// meanwhile.
    pub fn take_up(&self, pending: &Pending) {
        self.shared.update.lock().expect("update").command = Some(pending.id.clone());
    }

    /// Whether an update is under way: it starts the Runner again itself.
    fn update_under_way(&self) -> bool {
        self.shared.update.lock().expect("update").command.is_some()
    }

    /// Whether the Runner is to end so that the service manager starts it again.
    pub fn restart_requested(&self) -> bool {
        self.shared.restart.load(Ordering::SeqCst)
    }

    /// Ends [`Service::run`] so that the service manager starts the Runner again.
    fn restart(&self) {
        self.shared.restart.store(true, Ordering::SeqCst);
        self.shared.restarting.notify_one();
    }

    fn push_result(&self, id: String, ok: bool, message: String) {
        let shared = &self.shared;
        tracing::info!(%id, ok, %message, "a result for the console");
        let mut results = shared.results.lock().expect("results");
        // One command, one result: a Runner that starts again may come to say it once more.
        if results.iter().any(|result| result.id == id) {
            return;
        }
        // Never dropped: the console would send the command again, and it would be done again. Only the results that
        // come later by themselves (an update's, a test run's that waited for room) come while a report's worth
        // wait (see `answered`).
        results.push(CommandResult {
            id,
            ok,
            message: cut(message, STATUS_TEXT_MAX),
            at: iso(secs(shared.clock.now())),
        });
        self.keep(&results);
    }

    /// Writes the results that wait for the console to their file, when there is one.
    fn keep(&self, results: &[CommandResult]) {
        let Some(path) = &self.shared.results_file else {
            return;
        };
        let written = serde_json::to_vec(results)
            .map_err(|error| error.to_string())
            .and_then(|bytes| cf_secret::replace_private(path, &bytes).map_err(|error| error.to_string()));
        if let Err(error) = written {
            tracing::warn!(%error, "the results for the console could not be kept; a Runner that starts again does not say them");
        }
    }

    /// The scale sets that are served, by name.
    pub fn sets(&self) -> Vec<SetState> {
        let serving = self.shared.serving.lock().expect("serving");
        serving
            .values()
            .map(|s| s.state.lock().expect("set").clone())
            .collect()
    }

    /// Ends the listeners as a process that dies does: no session is closed, nothing is given back at GitHub.
    #[doc(hidden)]
    pub fn kill(&self) {
        for (_, serving) in std::mem::take(&mut *self.shared.serving.lock().expect("serving")) {
            serving.task.abort();
        }
    }

    /// What kept the last look at GitHub from being whole, when something did.
    pub fn discovery_error(&self) -> Option<String> {
        self.shared.discovery_error.lock().expect("discovery").clone()
    }

    /// Reads the repositories the App serves, their workflows and their variables, and serves one scale set per
    /// repository and set of labels whose size this Mac can run. A repository that could not be read keeps the
    /// scale sets it has; a scale set no workflow asks for any more is closed and, once no Actions runner of it
    /// is left, deleted.
    pub async fn discover(&self) {
        self.look(true).await;
    }

    /// Reads the jobs GitHub has queued and serves the label sets of those no workflow on a default branch names,
    /// as [`Service::discover`] does, on the repositories and workflows the last discovery read. Only a change of
    /// those sets reaches GitHub: one more to serve, one that expired.
    pub async fn look_at_queue(&self) {
        let Some(read) = self.shared.main.lock().expect("main").clone() else {
            return;
        };
        let before: BTreeMap<String, String> = self
            .shared
            .queued
            .lock()
            .expect("queued")
            .sets()
            .iter()
            .map(|(name, q)| (name.clone(), q.repo.clone()))
            .collect();
        let (queued, queue_problems, now_queued) = self.queued_wanted(&read).await;
        let mut problems = read.problems.clone();
        problems.extend(queue_problems);
        // Only the repositories whose sets changed need their scale sets listed.
        let mut changed: BTreeSet<String> = before
            .iter()
            .filter(|(name, _)| !queued.contains_key(*name))
            .map(|(_, repo)| repo.clone())
            .collect();
        changed.extend(
            queued
                .iter()
                .filter(|(name, _)| !before.contains_key(*name))
                .map(|(_, want)| want.repo.clone()),
        );
        if changed.is_empty() {
            self.say(problems, read.repos.len(), false);
        } else {
            let mut wanted = read.wanted.clone();
            wanted.extend(queued);
            self.apply(wanted, &read.unread, Some(&changed), problems, read.repos.len())
                .await;
        }
        self.held_jobs(&now_queued).await;
    }

    /// One look at GitHub. `wants`: whether this Mac still wants scale sets; when not, every one of its own goes.
    /// Says which scale sets of this Mac it could not delete.
    async fn look(&self, wants: bool) -> Vec<String> {
        let shared = &self.shared;
        let read = match self.read(wants).await {
            Ok(read) => read,
            Err(error) => {
                // Nothing is known: everything stays as it is.
                tracing::warn!(%error, "the App's installations could not be read");
                *shared.discovery_error.lock().expect("discovery") = Some(format!("GitHub: {error}"));
                shared.looked.store(true, Ordering::SeqCst);
                return vec!["GitHub could not be asked for this host's scale sets".into()];
            }
        };
        let mut wanted = read.wanted.clone();
        let mut problems = read.problems.clone();
        let now_queued = if wants {
            let (queued, queue_problems, queued_now) = self.queued_wanted(&read).await;
            wanted.extend(queued);
            problems.extend(queue_problems);
            queued_now
        } else {
            shared.queued.lock().expect("queued").clear();
            QueuedNow::default()
        };
        let read = Arc::new(read);
        *shared.main.lock().expect("main") = wants.then(|| read.clone());
        let kept = self
            .apply(wanted, &read.unread, None, problems, read.repos.len())
            .await;
        self.held_jobs(&now_queued).await;
        kept
    }

    /// Serves the scale sets `wanted` and closes, and once no Actions runner of it is left deletes, every other of
    /// this Mac at GitHub, but for those of a repository that could not be read. Says which it could not delete.
    async fn apply(
        &self,
        wanted: BTreeMap<String, Wanted>,
        unread: &BTreeSet<String>,
        only: Option<&BTreeSet<String>>,
        problems: Vec<String>,
        served: usize,
    ) -> Vec<String> {
        let shared = &self.shared;
        let machine = &shared.config.machine;

        // Listeners first, so a set one made at GitHub before it stopped is listed below and deleted with the rest.
        let unwanted: Vec<String> = shared
            .serving
            .lock()
            .expect("serving")
            .iter()
            .filter(|(name, serving)| {
                let repo = &serving.state.lock().expect("set").repo;
                !wanted.contains_key(*name)
                    && !unread.contains(repo)
                    && only.is_none_or(|only| only.contains(repo))
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in unwanted {
            self.close(&name).await;
        }

        // Scale sets of this Mac that nothing asks for any more, from this run of the Runner or one before.
        let clients: Vec<(String, ScaleSetClient)> = shared
            .clients
            .lock()
            .expect("clients")
            .iter()
            .map(|(repo, client)| (repo.clone(), client.clone()))
            .collect();
        let prefix = format!("{machine}--");
        let mut kept: Vec<String> = Vec::new();
        for (repo, client) in clients {
            if unread.contains(&repo) || only.is_some_and(|only| !only.contains(&repo)) {
                continue;
            }
            let sets = match client.list_scale_sets().await {
                Ok(sets) => sets,
                Err(error) => {
                    kept.push(format!("{repo}: its scale sets could not be listed ({error})"));
                    continue;
                }
            };
            for set in sets {
                if !set.name.starts_with(&prefix) || wanted.contains_key(&set.name) {
                    continue;
                }
                if shared.roster.active(&repo, set.id) == 0 {
                    match client.delete_scale_set(set.id).await {
                        Ok(_) => {
                            tracing::info!(%repo, set = %set.name, "a scale set nothing asks for was deleted")
                        }
                        Err(error) => {
                            tracing::debug!(%repo, set = %set.name, %error, "a scale set could not be deleted yet");
                            kept.push(format!("{repo}: scale set {} ({error})", set.name));
                        }
                    }
                } else {
                    kept.push(format!(
                        "{repo}: scale set {} still has Actions runners",
                        set.name
                    ));
                }
            }
        }
        for (name, want) in wanted {
            let mut serving = shared.serving.lock().expect("serving");
            if serving.contains_key(&name) {
                continue;
            }
            let state = Arc::new(Mutex::new(SetState {
                repo: want.repo.clone(),
                name: name.clone(),
                id: 0,
                labels: want.labels.clone(),
                size: want.size,
                docker: want.docker,
                error: None,
                error_at: None,
                statistics: None,
                never: None,
                room_since: None,
            }));
            let (stop, stopped) = cf_scaleset::shutdown();
            let task = tokio::spawn(serve(shared.clone(), want.client.clone(), state.clone(), stopped));
            serving.insert(name, Serving { stop, task, state });
        }
        kept.extend(self.say(problems, served, true));
        kept
    }

    /// Keeps what a look found wrong for the console, and logs it: `always`, or when it is news.
    fn say(&self, problems: Vec<String>, served: usize, always: bool) -> Vec<String> {
        let shared = &self.shared;
        shared.looked.store(true, Ordering::SeqCst);
        *shared.not_serving.lock().expect("not serving") = (served == 0 && problems.is_empty())
            .then(|| "the App is installed on no repository this host serves".to_string());
        let said = (!problems.is_empty()).then(|| problems.join("; "));
        let before = std::mem::replace(
            &mut *shared.discovery_error.lock().expect("discovery"),
            said.clone(),
        );
        if always || before != said {
            for problem in &problems {
                tracing::warn!(%problem, "discovery");
            }
        }
        problems
    }

    /// The scale sets of queued jobs to serve beside main's, from a new look at the queue and those seen before,
    /// and what went wrong or was left out.
    async fn queued_wanted(&self, read: &Read) -> (BTreeMap<String, Wanted>, Vec<String>, QueuedNow) {
        let shared = &self.shared;
        let QueueRead {
            seen,
            queued: now_queued,
            unread,
            mut problems,
        } = self.read_queue(read).await;
        let unread: BTreeSet<String> = read.unread.iter().cloned().chain(unread).collect();
        let served: BTreeSet<String> = read
            .repos
            .iter()
            .map(|(repo, _)| repo.full_name.clone())
            .collect();
        let main: BTreeSet<String> = read.wanted.keys().cloned().collect();
        let (per_repo, per_host) = shared.config.queued_bounds;
        let bounds = Bounds {
            keep: shared.config.queued_keep,
            per_repo,
            per_host,
        };
        let look = Look {
            now: shared.clock.now(),
            served: &served,
            unread: &unread,
            main: &main,
        };
        let settings = shared.machines.settings();
        let mut queued = shared.queued.lock().expect("queued");
        for (name, set) in seen {
            queued.seen(name, set);
        }
        // What the settings or the policy stopped serving since a set was seen goes now, as main's would.
        queued.retain(
            |q| match read.repos.iter().find(|(repo, _)| repo.full_name == q.repo) {
                Some((repo, _)) => self.size_here(repo, &q.set, &settings) == Ok(q.size),
                None => true,
            },
        );
        problems.extend(queued.select(&look, bounds));
        let clients = shared.clients.lock().expect("clients");
        let wanted = queued
            .sets()
            .iter()
            .filter_map(|(name, q)| {
                let want = Wanted {
                    repo: q.repo.clone(),
                    client: clients.get(&q.repo)?.clone(),
                    labels: q.set.labels().to_vec(),
                    size: q.size,
                    docker: q.set.docker(),
                };
                Some((name.clone(), want))
            })
            .collect();
        (wanted, problems, now_queued)
    }

    /// Reads the jobs GitHub has queued or runs in the repositories `read` served and read, for the label sets this
    /// host would serve that main does not name. It only reads, [`QUEUE_READS`] repositories at once. The whole
    /// look takes at most [`ServiceConfig::queue_budget`]: a repository not read by then counts as not read, and
    /// the next look starts with it.
    async fn read_queue(&self, read: &Read) -> QueueRead {
        let shared = &self.shared;
        let machine = &shared.config.machine;
        let settings = shared.machines.settings();
        let now = shared.clock.now();
        let deadline = tokio::time::Instant::now() + shared.config.queue_budget;
        let repos: Vec<&(Repo, i64)> = read
            .repos
            .iter()
            .filter(|(repo, _)| !read.unread.contains(&repo.full_name))
            .collect();
        let first = shared.queue_cursor.load(Ordering::Relaxed) % repos.len().max(1);
        let order: Vec<usize> = (0..repos.len()).map(|k| (first + k) % repos.len()).collect();
        let mut answers: Vec<Option<Result<cf_ghapp::Waiting, cf_ghapp::Error>>> =
            repos.iter().map(|_| None).collect();
        let mut next = order.iter().copied();
        let mut reads = tokio::task::JoinSet::new();
        loop {
            while reads.len() < QUEUE_READS
                && let Some(i) = next.next()
            {
                let (app, installation, name) =
                    (shared.app.clone(), repos[i].1, repos[i].0.full_name.clone());
                reads.spawn(async move { (i, app.waiting_jobs(installation, &name).await) });
            }
            match tokio::time::timeout_at(deadline, reads.join_next()).await {
                Ok(Some(Ok((i, answer)))) => answers[i] = Some(answer),
                // A read that panicked counts as one not made in time.
                Ok(Some(Err(error))) => tracing::error!(%error, "a read of a queue failed"),
                Ok(None) | Err(_) => break,
            }
        }
        // Out of time: the reads still going end here.
        drop(reads);
        // What the queue of a repository no longer served last read as is not needed again.
        shared
            .app
            .forget_queues(|name| read.repos.iter().any(|(repo, _)| repo.full_name == name));
        if let Some(&i) = order.iter().find(|&&i| answers[i].is_none()) {
            shared.queue_cursor.store(i, Ordering::Relaxed);
        }
        let mut queue = QueueRead::default();
        let mut late: Vec<String> = Vec::new();
        for ((repo, _), answer) in repos.into_iter().zip(answers) {
            let name = &repo.full_name;
            let waiting = match answer {
                Some(Ok(waiting)) => waiting,
                // Refused, not limited: a rate limit is no missing permission.
                Some(Err(cf_ghapp::Error::Status(refused)))
                    if matches!(refused.status, 403 | 404) && !refused.is_rate_limited() =>
                {
                    queue.problems.push(format!(
                        "{name}: its queued jobs could not be read ({refused}); only the label sets of the \
                         workflows on its default branch are served (the App needs \"Actions: read\")"
                    ));
                    queue.unread.insert(name.clone());
                    continue;
                }
                Some(Err(error)) => {
                    queue
                        .problems
                        .push(format!("{name}: its queued jobs could not be read ({error})"));
                    queue.unread.insert(name.clone());
                    continue;
                }
                None => {
                    late.push(name.clone());
                    queue.unread.insert(name.clone());
                    continue;
                }
            };
            for why in &waiting.left_out {
                tracing::info!(repo = %name, %why, "a queued job's labels were not read");
            }
            queue.queued.read.insert(name.clone());
            for job in waiting.jobs {
                let set = job.set;
                let set_name = scale_set_name(machine, name, &set);
                // A fork's job too: it runs on main's sets.
                if !job.queued.is_empty() {
                    let queued = queue
                        .queued
                        .jobs
                        .entry(set_name.clone())
                        .or_insert_with(|| QueuedJobs {
                            repo: name.clone(),
                            labels: set.labels().to_vec(),
                            jobs: Vec::new(),
                        });
                    queued.jobs.extend(job.queued);
                }
                // A public repository's pull request from a fork is a stranger's code: it runs on main's sets, if
                // its labels are theirs, but makes none of its own.
                if job.from_fork && !repo.private {
                    tracing::debug!(repo = %name, job = job.id, "a fork's queued job makes no set on a public repository");
                    continue;
                }
                if read.wanted.contains_key(&set_name) {
                    continue;
                }
                match self.size_here(repo, &set, &settings) {
                    Ok(size) => queue.seen.push((
                        set_name,
                        QueuedSet {
                            repo: name.clone(),
                            set,
                            size,
                            last_seen: now,
                            newest: (job.created, job.id),
                        },
                    )),
                    Err(why) => {
                        tracing::debug!(repo = %name, labels = %set.key(), why = why.unwrap_or("no size"), "a queued job's set is not served")
                    }
                }
            }
        }
        if !late.is_empty() {
            queue.problems.push(format!(
                "the queued jobs of {} were not read in time",
                late.join(", ")
            ));
        }
        for queued in queue.queued.jobs.values_mut() {
            queued.jobs.sort_unstable();
        }
        queue
    }

    /// The size of the machines this host serves a repository's label set with, or why it serves none: none when
    /// the set names no size, which needs no word.
    fn size_here(
        &self,
        repo: &Repo,
        set: &LabelSet,
        settings: &cf_budget::Settings,
    ) -> Result<Size, Option<&'static str>> {
        let policy = &self.shared.config.policy;
        if set.docker() && self.docker_refused().is_some() {
            // Not offered to GitHub while the host's own check says its machines start no Docker daemon.
            return Err(Some(crate::docker_check::REFUSAL));
        }
        if set.docker() && !policy.serves_docker(repo) {
            // Not offered to GitHub: the job waits for a Mac that gives this repository Docker. A Mac left out on
            // purpose is no problem of discovery.
            return Err(Some("Docker is only for the repositories in docker_repos"));
        }
        if !policy.serves_labels(set) {
            // Pinned to hosts of another kind: left to them, as a size this host never runs.
            return Err(Some("it asks for host labels this host does not have"));
        }
        // `cf-docker` alone is a machine of the size `self-hosted` means.
        let size = Size::of_labels(set.labels().iter().map(String::as_str))
            .or_else(|| set.docker().then_some(Size::G4))
            .ok_or(None)?;
        // Not offered to GitHub at all: a job nobody here can run is left to the other Macs.
        settings.can_run(size).map_err(|refusal| Some(refusal.name()))?;
        Ok(size)
    }

    /// What GitHub shows of the repositories this Mac serves: the scale sets their workflows ask for (none when
    /// the Mac wants none), the repositories that could not be read, and what went wrong. It only reads. An
    /// error: GitHub did not even list the App's installations.
    async fn read(&self, wants: bool) -> Result<Read, cf_ghapp::Error> {
        let shared = &self.shared;
        let machine = &shared.config.machine;
        let settings = shared.machines.settings();
        let mut wanted: BTreeMap<String, Wanted> = BTreeMap::new();
        let mut unread: BTreeSet<String> = BTreeSet::new();
        let mut problems: Vec<String> = Vec::new();
        let mut served: Vec<(Repo, i64)> = Vec::new();
        let installations = shared.app.installations().await?;
        for installation in installations {
            let repos = match shared.app.repositories(installation.id).await {
                Ok(repos) => repos,
                Err(error) => {
                    problems.push(format!("installation {}: {error}", installation.id));
                    // Whichever repositories it has keep their scale sets.
                    let serving = shared.serving.lock().expect("serving");
                    unread.extend(
                        serving
                            .values()
                            .map(|s| s.state.lock().expect("set").repo.clone()),
                    );
                    continue;
                }
            };
            for repo in repos {
                if !shared.config.policy.serves(&repo) {
                    continue;
                }
                served.push((repo.clone(), installation.id));
                let name = repo.full_name.clone();
                let Some(client) = shared.client_of(installation.id, &name) else {
                    problems.push(format!(
                        "{name}: scale sets need the console, and this Runner is not enrolled"
                    ));
                    continue;
                };
                let files = match shared
                    .app
                    .workflow_files(installation.id, &name, shared.config.max_workflow_bytes)
                    .await
                {
                    Ok(files) => files,
                    Err(error) => {
                        problems.push(format!("{name}: its workflows could not be read ({error})"));
                        unread.insert(name);
                        continue;
                    }
                };
                // Needs the App's "Variables: read" permission. Without it a label that comes from a variable
                // is not known and stands for the pool's label, as any value does that names no size.
                let variables = match shared.app.variables(installation.id, &name).await {
                    Ok(variables) => variables,
                    Err(cf_ghapp::Error::Status(refused)) if matches!(refused.status, 403 | 404) => {
                        problems.push(format!(
                            "{name}: its variables could not be read ({refused}); a `runs-on` that takes its \
                             label from a variable counts as `self-hosted`"
                        ));
                        BTreeMap::new()
                    }
                    // GitHub did not answer, which says nothing of the variables: the scale sets made from them
                    // stay as they are until it does.
                    Err(error) => {
                        problems.push(format!("{name}: its variables could not be read ({error})"));
                        unread.insert(name);
                        continue;
                    }
                };
                if !wants {
                    continue;
                }
                for set in self_hosted_with(&files, &variables) {
                    let size = match self.size_here(&repo, &set, &settings) {
                        Ok(size) => size,
                        Err(Some(why)) => {
                            tracing::info!(repo = %name, labels = %set.key(), %why, "a label set is not served");
                            continue;
                        }
                        Err(None) => continue,
                    };
                    wanted.insert(
                        scale_set_name(machine, &name, &set),
                        Wanted {
                            repo: name.clone(),
                            client: client.clone(),
                            labels: set.labels().to_vec(),
                            size,
                            docker: set.docker(),
                        },
                    );
                }
            }
        }
        Ok(Read {
            wanted,
            unread,
            problems,
            repos: served,
        })
    }

    /// What this Mac would serve, read from GitHub and nothing else: no scale set is made, no session opened, no
    /// Actions runner registered. Each line is a repository and the labels of one scale set with its size; the
    /// second list is what could not be read. For a check before the service is installed: a missing permission
    /// of the App (its variables, its workflows) shows here.
    pub async fn survey(&self) -> (Vec<String>, Vec<String>) {
        let read = match self.read(true).await {
            Ok(read) => read,
            Err(error) => return (Vec::new(), vec![format!("GitHub: {error}")]),
        };
        let queue = self.read_queue(&read).await;
        let mut lines: Vec<String> = read
            .wanted
            .values()
            .map(|want| {
                format!(
                    "{}: [{}] as {}",
                    want.repo,
                    want.labels.join(", "),
                    want.size.label()
                )
            })
            .collect();
        let queued: BTreeMap<&String, &QueuedSet> = queue.seen.iter().map(|(name, q)| (name, q)).collect();
        lines.extend(queued.values().map(|q| {
            format!(
                "{}: [{}] as {}, for queued jobs",
                q.repo,
                q.set.labels().join(", "),
                q.size.label()
            )
        }));
        let mut problems = read.problems;
        problems.extend(queue.problems);
        (lines, problems)
    }

    /// The client of a repository no look at GitHub has shown since the Runner started: a registration of before
    /// may be all that is left of it here.
    async fn client_for(&self, repo: &str) -> Option<ScaleSetClient> {
        let shared = &self.shared;
        if let Some(client) = shared.clients.lock().expect("clients").get(repo) {
            return Some(client.clone());
        }
        shared.console.as_ref()?;
        let installation = shared.app.repo_installation(repo).await.ok()?;
        let found = shared
            .app
            .repositories(installation)
            .await
            .ok()?
            .into_iter()
            .find(|found| found.full_name.eq_ignore_ascii_case(repo))?;
        shared.client_of(installation, &found.full_name)
    }

    /// Finds the jobs GitHub holds back from this host's scale sets while they have room ([`crate::held`]), keeps
    /// them for the report, retires the Actions runners of their own that have nothing left to take, and gives
    /// each scale set with held jobs one of its own when a slot of its size is free. A paused host (the console's
    /// pause is [`Machines::pause`]) and a full one give no slot, so they start none.
    async fn held_jobs(&self, queued: &QueuedNow) {
        let shared = &self.shared;
        let now = shared.clock.now();
        let sets = self.sets();
        let views: Vec<SetView> = sets
            .iter()
            .filter(|set| queued.read.contains(&set.repo))
            .map(|set| SetView {
                name: set.name.clone(),
                repo: set.repo.clone(),
                labels: set.labels.clone(),
                room_since: set.room_since,
                assigned_unstarted: set.statistics.as_ref().map_or(0, Statistics::unstarted),
                queued: queued
                    .jobs
                    .get(&set.name)
                    .map(|queued| queued.jobs.clone())
                    .unwrap_or_default(),
            })
            .collect();
        let found = held::held(now, &views);
        // The slots are taken, and the tries counted, under the lock: a look at the same time starts none twice.
        let starts: Vec<(&SetState, &HeldSet, Ticket)> = {
            let mut held = shared.held.lock().expect("held");
            held.own.retain(|name| sets.iter().any(|set| set.name == name));
            self.retire_own(&held.own, &sets, &views, now);
            held.sets.clone_from(&found);
            let mut starts = Vec::new();
            // One that took a job waits for none: the jobs held behind it get another.
            let waits = |runner: &str| shared.roster.get(runner).is_some_and(|runner| runner.waits());
            for set in &found {
                let Some(state) = sets.iter().find(|state| state.name == set.name) else {
                    continue;
                };
                if !held.own.due(now, &set.name, waits) {
                    continue;
                }
                // It carries all the set's labels, so GitHub could give it a job that asks for only some of them,
                // and wants a machine of another size.
                let others = queued
                    .jobs
                    .values()
                    .map(|other| (other.repo.as_str(), other.labels.as_slice()));
                if held::narrower_waits(&set.repo, &set.labels, others) {
                    held.own.tried(now, &set.name, String::new());
                    tracing::info!(
                        repo = %set.repo,
                        labels = %set.labels.join(","),
                        job = set.oldest,
                        "GitHub holds a job back from a scale set with room, but no Actions runner of its own is \
                         started for it: a queued job of the repository with only some of its labels could go to it"
                    );
                    continue;
                }
                if let Some(ticket) = self.own_slot(state) {
                    held.own.tried(now, &set.name, String::new());
                    starts.push((state, set, ticket));
                }
            }
            starts
        };
        for (state, set, ticket) in starts {
            if let Some(runner) = self.own_runner(state, set, ticket).await {
                shared
                    .held
                    .lock()
                    .expect("held")
                    .own
                    .tried(now, &set.name, runner);
            }
        }
    }

    /// Asks for the removal of the Actions runners of their own that wait for a job when their scale set's queue
    /// holds none they could take, or they waited [`held::OWN_WAIT`]. One that took a job is refused by GitHub, and
    /// one that is not idle any more is left as it is (`Hosts::retire`).
    fn retire_own(&self, own: &OwnRunners, sets: &[SetState], views: &[SetView], now: SystemTime) {
        let shared = &self.shared;
        let repos: BTreeSet<&str> = sets.iter().map(|set| set.repo.as_str()).collect();
        for repo in repos {
            let idle: Vec<String> = shared
                .roster
                .runners(repo, held::OWN)
                .into_iter()
                .filter(Runner::waits)
                .filter(|runner| {
                    let unassigned = own
                        .set_of(&runner.name)
                        .and_then(|set| views.iter().find(|view| view.name == set))
                        .map(|view| view.unassigned().len());
                    held::own_done(now.duration_since(runner.started).unwrap_or_default(), unassigned)
                })
                .map(|runner| runner.name)
                .collect();
            // Every one of them: those of the repository's other scale sets that already ask are no surplus of these,
            // and would hold them back if counted against them.
            if !idle.is_empty() {
                Hosts::retire(&*shared.machines, repo, held::OWN, usize::MAX, &idle);
            }
        }
    }

    /// A slot for an Actions runner of a scale set's own, as the budget gives one to any job it is offered: none
    /// while the host is paused, full, or someone works on it.
    fn own_slot(&self, state: &SetState) -> Option<Ticket> {
        let machines = &*self.shared.machines;
        let promised = machines.reserve_for(state.size, state.docker).ok()?;
        match machines.claim(state.size, Some(promised)) {
            Claim::Start(ticket) => {
                if state.docker {
                    machines.mark_docker(ticket);
                }
                Some(ticket)
            }
            Claim::Waits { ticket, .. } => {
                machines.withdraw(ticket);
                None
            }
            Claim::Never(_) => None,
        }
    }

    /// Starts an Actions runner of its own for a scale set's held jobs in the slot `ticket`, registered outside the
    /// scale set with exactly its labels. Says its name; none when GitHub would not register it, and the slot is
    /// then given back.
    async fn own_runner(&self, state: &SetState, set: &HeldSet, ticket: Ticket) -> Option<String> {
        let shared = &self.shared;
        let machines = &*shared.machines;
        let Some(client) = self.client_for(&state.repo).await else {
            machines.release(ticket);
            return None;
        };
        let registry = SetRegistry::own(client, state.labels.clone());
        let at = Start {
            machine: &shared.config.machine,
            repo: &state.repo,
            scale_set_id: held::OWN,
            size: state.size,
        };
        match start_runner(&registry, machines, &shared.roster, &*shared.clock, at, ticket).await {
            Ok(runner) => {
                tracing::info!(
                    %runner, repo = %set.repo, labels = %set.labels.join(","), job = set.oldest, jobs = set.jobs,
                    waited = shared.clock.now().duration_since(set.queued).unwrap_or_default().as_secs(),
                    room = set.room.as_secs(),
                    "GitHub holds a job back from a scale set with room: an Actions runner of its own takes it"
                );
                Some(runner)
            }
            Err(error) => {
                machines.release(ticket);
                tracing::warn!(
                    repo = %set.repo, labels = %set.labels.join(","), %error,
                    "an Actions runner of its own for a held job could not start"
                );
                None
            }
        }
    }

    /// Ends everything this Mac has, for an uninstall: no new job is taken, every machine is stopped, every
    /// registration and scale set of this Mac is removed at GitHub, and the console hears of the jobs that ended
    /// (and, with `leave`, that the Mac is gone: its token is void after). Waits at most `wait` for the machines
    /// to go and for GitHub to take the registrations back. How GitHub calls the end of a job that was stopped is
    /// not waited for: the sessions are closed, so its word cannot come, and the job is reported as `stopped`.
    /// Says what is left; nothing when all went.
    ///
    /// The machines' tick is the caller's, as in [`Service::run`]: without it no machine is seen to go.
    pub async fn retire(&self, wait: Duration, leave: bool) -> Vec<String> {
        let shared = &self.shared;
        shared.machines.drain();
        self.close_all().await;
        shared.machines.hear_no_more();
        shared.machines.stop_all();
        let deadline = Instant::now() + wait;
        loop {
            self.sweep().await;
            // Until nothing is left to stop, to unregister or to report.
            if shared.machines.unsettled() == 0 || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(shared.config.sweep_again.min(Duration::from_millis(500))).await;
        }
        let mut left: Vec<String> = Vec::new();
        for machine in shared.machines.snapshot().machines {
            left.push(format!("the machine of {} still stands", machine.runner));
        }
        for registration in shared.machines.registrations_to_remove() {
            left.push(format!(
                "{}: the Actions runner {} is still registered",
                registration.repo, registration.runner
            ));
        }
        if left.is_empty() && shared.machines.unsettled() > 0 {
            left.push(format!(
                "{} jobs whose end is not reported yet",
                shared.machines.unsettled()
            ));
        }
        left.extend(self.look(false).await);
        if let Some(console) = &shared.console {
            // One report carries one batch of the outbox: go on while the console takes them.
            let drained = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    self.report().await;
                    let taken = shared.told.lock().expect("told").1;
                    if console.outbox.is_empty() || !taken {
                        break;
                    }
                }
            })
            .await;
            if drained.is_err() || !console.outbox.is_empty() {
                left.push(format!(
                    "{} job events the console has not heard",
                    console.outbox.len()
                ));
            }
            if leave && let Err(error) = console.client.leave().await {
                left.push(format!(
                    "the console was not told that the host is gone ({error})"
                ));
            }
        }
        left
    }

    /// Ends a scale set's listener and waits for its session to be closed.
    async fn close(&self, name: &str) {
        let serving = self.shared.serving.lock().expect("serving").remove(name);
        if let Some(serving) = serving {
            serving.stop.trigger();
            if tokio::time::timeout(Duration::from_secs(10), serving.task)
                .await
                .is_err()
            {
                tracing::warn!(set = %name, "a listener did not end in time");
            }
        }
    }

    /// Removes at GitHub the registrations the machines name: of machines that are gone and of Actions runners
    /// that never got one. One GitHub would not remove (its job is still GitHub's to end, or GitHub is away) is
    /// tried again later, for as long as it takes.
    pub async fn sweep(&self) {
        let shared = &self.shared;
        for registration in shared.machines.registrations_to_remove() {
            let waits = shared
                .refused
                .lock()
                .expect("refused")
                .get(&registration.runner)
                .is_some_and(|until| Instant::now() < *until);
            if waits {
                continue;
            }
            let Some(client) = self.client_for(&registration.repo).await else {
                // The App no longer reads its repository, or GitHub is away: asked again later.
                tracing::debug!(runner = %registration.runner, repo = %registration.repo, "no client for a registration yet");
                self.sweep_later(registration.runner);
                continue;
            };
            // Removed by its name, which the console checks is this machine's: an ID heard or not, the name is the
            // registration's. GitHub's answer is what proves anything (`Machines::answered`); no answer (a 5xx, a
            // timeout, a dropped connection) proves nothing, and the removal is sent again at a later sweep.
            let answer = match client.remove_runner_named(&registration.runner).await {
                Ok(true) => Ok(machines::Answer::Removed),
                Ok(false) => Ok(machines::Answer::Gone),
                Err(error) if error.status() == Some(422) => Ok(machines::Answer::Refused),
                Err(error) => Err(error),
            };
            match answer {
                Ok(answer) => {
                    if answer == machines::Answer::Refused {
                        // The job holds it until it is over: asked again later.
                        self.sweep_later(registration.runner.clone());
                    } else {
                        shared
                            .refused
                            .lock()
                            .expect("refused")
                            .remove(&registration.runner);
                    }
                    shared.machines.answered(&registration.runner, answer);
                }
                Err(error) => {
                    tracing::warn!(runner = %registration.runner, %error, "a registration could not be removed; it is tried again");
                    self.sweep_later(registration.runner);
                }
            }
        }
    }

    /// Leaves a registration to a later sweep.
    fn sweep_later(&self, runner: String) {
        let again = Instant::now() + self.shared.config.sweep_again;
        self.shared.refused.lock().expect("refused").insert(runner, again);
    }

    /// Why jobs GitHub handed to this Mac wait while nothing runs here, when they do. Jobs GitHub still holds are
    /// not seen: its counts never name them (`totalAvailableJobs` is 0 in every count the probes of 2026-10-12 saw),
    /// and only it decides when to hand them over (`docs/github-protocol.md`).
    fn stall_reason(&self, sets: &[SetState], snapshot: &Snapshot) -> Option<String> {
        let shared = &self.shared;
        if *shared.console_paused.lock().expect("paused") {
            return None;
        }
        if snapshot.waiting == 0 || !snapshot.machines.is_empty() {
            return None;
        }
        // A Mac with jobs in line is closed to the next one for them; what keeps the first of them, which goes before
        // the others, is what the console needs: memory, or room made for another size.
        let why = match snapshot.budget.closed_by {
            None | Some(cf_budget::Refusal::AssignedFirst { .. }) => snapshot
                .budget
                .waiting
                .first()
                .and_then(|waiter| waiter.why)
                .map(|why| why.name())
                .or_else(|| sets.iter().find_map(|set| snapshot.short_of(set.size)))
                .unwrap_or("no machine of their size fits"),
            Some(closed) => closed.name(),
        };
        Some(format!("nothing runs and no job starts: {why}"))
    }

    /// For how long jobs have waited while the Mac could run them, and why it believes they do: warns once when
    /// that passes [`STALL_WARN_AFTER`], and says when it is over.
    fn watch_stall(&self, sets: &[SetState], snapshot: &Snapshot) -> (u64, Option<String>) {
        let now = self.shared.clock.now();
        let reason = self.stall_reason(sets, snapshot);
        let mut stall = self.shared.stall.lock().expect("stall");
        let Some(reason) = reason else {
            if let Some(over) = stall.take().filter(|over| over.warned) {
                let seconds = now.duration_since(over.since).unwrap_or_default().as_secs();
                tracing::info!(
                    seconds,
                    "the jobs that waited while the host was idle are under way or gone"
                );
            }
            return (0, None);
        };
        let stall = stall.get_or_insert(Stall {
            since: now,
            warned: false,
        });
        let waited = now.duration_since(stall.since).unwrap_or_default();
        if waited >= STALL_WARN_AFTER && !stall.warned {
            stall.warned = true;
            tracing::warn!(seconds = waited.as_secs(), %reason, "idle with jobs waiting");
        }
        (waited.as_secs(), Some(reason))
    }

    /// What the console is told now, without the job events.
    pub fn status(&self) -> Report {
        let shared = &self.shared;
        let snapshot = shared.machines.snapshot();
        let extras = (shared.extras)();
        let sets = self.sets();
        let settings = shared.machines.settings();
        let now = secs(shared.clock.now());
        let host = snapshot.budget.reading.map(|reading| {
            let total = settings.total_bytes.max(1);
            HostStatus {
                cpus: settings.cores,
                load1: extras.load[0],
                load5: extras.load[1],
                load15: extras.load[2],
                #[allow(clippy::cast_precision_loss)]
                free_memory_percent: (reading.available_bytes.min(total) as f64 / total as f64 * 100.0),
                available_memory_bytes: reading.available_bytes,
                memory_floor_bytes: snapshot.budget.limits.floor_bytes,
                memory_pressure: match reading.pressure {
                    Pressure::Normal => 1,
                    Pressure::Warning => 2,
                    Pressure::Critical => 4,
                },
                memory_bytes: settings.total_bytes,
                swap_used_bytes: reading.swap_used_bytes,
                swap_total_bytes: extras.swap_total_bytes.max(reading.swap_used_bytes),
                disk_free_bytes: reading.disk_free_bytes,
                disk_total_bytes: extras.disk_total_bytes.max(reading.disk_free_bytes),
                boot_time: extras.boot.map(iso),
                #[allow(clippy::cast_precision_loss)]
                idle_seconds: reading.idle.0 as f64 / 1000.0,
            }
        });
        let paused = *shared.console_paused.lock().expect("paused");
        let (idle_with_jobs_waiting_seconds, idle_with_jobs_waiting_reason) =
            self.watch_stall(&sets, &snapshot);
        // A Mac the budget calls open takes no job while what runs on it leaves no memory for even the smallest
        // size it serves: the console hears that it is closed and what for, in the budget's word.
        let no_room = sets
            .iter()
            .map(|set| set.size)
            .min_by_key(|size| size.index())
            .and_then(|smallest| snapshot.short_of(smallest));
        let updating = shared.update.lock().expect("update");
        let (open, reason) = match (&updating.doing, no_room) {
            (Some(doing), _) => (false, Some(doing.clone())),
            (None, Some(why)) => (false, Some(why.to_string())),
            (None, None) => (snapshot.open, (!snapshot.open).then(|| snapshot.reason.clone())),
        };
        let update = shared.updates.as_ref().map(|updates| Update {
            source: updates.source(),
            moved_from: None,
            checked_at: updating.asked_at.map(iso),
            error: updating
                .error
                .as_ref()
                .map(|(error, _)| cut(error.clone(), STATUS_TEXT_MAX)),
            error_at: updating.error.as_ref().map(|(_, at)| iso(*at)),
            failures: updating.failures,
        });
        drop(updating);
        let settings_error = shared.applying.lock().expect("applying").warning.clone();
        let not_serving = if shared.ending.load(Ordering::SeqCst) {
            Some("the Runner ends; the one that starts next serves them again".to_string())
        } else {
            shared.not_serving.lock().expect("not serving").clone()
        };
        Report {
            status: Status {
                machine: shared.config.machine.clone(),
                version: shared.config.version.clone(),
                updated_at: iso(now),
                open,
                reason,
                // Of the commonest size, under the limits in force, for consoles that do not read the memory.
                max_jobs: settings.side_by_side(&snapshot.budget.limits, Size::G4),
                memory_for_jobs_bytes: Some(snapshot.budget.limits.cap_bytes),
                paused,
                host,
                host_error: snapshot
                    .host_error
                    .clone()
                    .or_else(|| {
                        (!snapshot.stuck.is_empty())
                            .then(|| format!("a vm-host does not end: {}", snapshot.stuck.join(", ")))
                    })
                    .map(|error| cut(error, HOST_ERROR_MAX)),
                discovery_error: self.discovery_error().map(|error| cut(error, STATUS_TEXT_MAX)),
                update,
                not_serving,
                starting: !shared.looked.load(Ordering::SeqCst),
                app_key_file: held_key_file(shared.config.app_key_file.as_deref()),
                settings_error: settings_error
                    .as_ref()
                    .map(|(error, _)| cut(error.clone(), STATUS_TEXT_MAX)),
                settings_error_at: settings_error.as_ref().map(|(_, at)| iso(*at)),
                idle_with_jobs_waiting_seconds,
                idle_with_jobs_waiting_reason: idle_with_jobs_waiting_reason
                    .map(|reason| cut(reason, STATUS_TEXT_MAX)),
                sizes: snapshot
                    .rooms
                    .iter()
                    .map(|room| SizeStatus {
                        size: room.size.label().to_string(),
                        free: room.free.map_or(-1, i64::from),
                        why: room.why.map(str::to_string),
                        need_bytes: room.need_bytes,
                        short_bytes: room.short_bytes,
                    })
                    .collect(),
                commands: self.commands(),
                held_jobs: self.held_report(),
                docker: shared.machines.docker_check().map(|check| DockerStatus {
                    ok: check.ok,
                    checked_at: iso(check.at),
                    why: check.why.map(|why| cut(why, crate::docker_check::WHY_MAX)),
                }),
                scale_sets: sets
                    .iter()
                    .take(MAX_SCALE_SETS)
                    .map(|set| ScaleSetStatus {
                        repo: set.repo.clone(),
                        name: set.name.clone(),
                        id: set.id,
                        labels: set.labels.iter().take(MAX_LABELS).cloned().collect(),
                        capacity: snapshot.room(set.size).free.unwrap_or(0),
                        waiting: set.statistics.as_ref().map_or(0, |s| {
                            u32::try_from(s.total_available_jobs.max(0)).unwrap_or(u32::MAX)
                        }),
                        error: set
                            .error
                            .clone()
                            .or_else(|| {
                                set.never.as_ref().map(|why| {
                                    format!(
                                        "a job of {} can never start on this host: {why}",
                                        set.size.label()
                                    )
                                })
                            })
                            .map(|error| cut(error, STATUS_TEXT_MAX)),
                        error_at: set.error_at.map(iso),
                    })
                    .collect(),
                runners: snapshot
                    .machines
                    .iter()
                    .take(MAX_RUNNERS)
                    .map(|machine| RunnerStatus {
                        name: machine.runner.clone(),
                        scale_set: machine.scale_set_id,
                        state: machine.state.to_string(),
                        started: iso(machine.started),
                        job: machine.github.as_ref().map(|job| RunnerJob {
                            repo: job.repo.clone(),
                            name: job.job.clone(),
                            workflow: job.workflow.clone(),
                            run_id: u64::try_from(job.run_id).unwrap_or(0),
                            started: iso(job.started_at.unwrap_or(machine.started)),
                        }),
                        usage: machine.used_bytes.map(|memory_bytes| RunnerUsage {
                            cpu_percent: machine.cpu_percent.map_or(-1.0, f64::from),
                            memory_bytes,
                            file_bytes: machine.file_bytes,
                            stalls: machine.pressure.map(stalls),
                            host_cpu: machine.host_cpu.map(share),
                        }),
                        size: machine.size.map(|size| size.label().to_string()),
                        counted_bytes: machine.counted_bytes,
                    })
                    .collect(),
            },
            activity: Activity {
                cpu_percent: extras.cpu_percent,
                power: extras.battery.map(|battery| Power {
                    on_battery: battery.on_battery,
                    percent: battery.percent,
                    charging: battery.charging,
                }),
            },
            hostname: shared.config.hostname.clone(),
            model: shared.config.model.clone(),
            os: shared.config.os,
            settings: None,
            limits: self.limits(&settings),
            image: None,
            jobs: Vec::new(),
            results: Vec::new(),
        }
    }

    /// The jobs GitHub held back as the last look found them, each wait as of now.
    fn held_report(&self) -> Vec<HeldJobs> {
        let held = self.shared.held.lock().expect("held");
        let now = self.shared.clock.now();
        held.sets
            .iter()
            .map(|set| HeldJobs {
                repo: set.repo.clone(),
                labels: set.labels.iter().take(MAX_LABELS).cloned().collect(),
                jobs: u32::try_from(set.jobs).unwrap_or(u32::MAX),
                oldest_wait_seconds: now.duration_since(set.queued).unwrap_or_default().as_secs(),
            })
            .collect()
    }

    /// Reports to the console, with the job events that wait, and does what its answer says. Returns when the
    /// console wants to hear again. A console that is away loses nothing: the job events wait in the outbox.
    pub async fn report(&self) -> Duration {
        let shared = &self.shared;
        let Some(console) = &shared.console else {
            return shared.config.report_every;
        };
        let mut report = self.status();
        // Kept until the console has them, so that one said meanwhile is written down beside them. The oldest go
        // first, as many as a report carries; the rest go with the next.
        let results: Vec<CommandResult> = shared
            .results
            .lock()
            .expect("results")
            .iter()
            .take(cf_console_client::MAX_RESULTS)
            .cloned()
            .collect();
        report.results.clone_from(&results);
        // Read before the events are taken for the report: one queued meanwhile is news after it.
        let queued = console.outbox.queued();
        let delivered = console.client.deliver(&console.outbox, report).await;
        *shared.told.lock().expect("told") = (queued, delivered.is_ok());
        match delivered {
            Ok(answer) => {
                if !results.is_empty() {
                    // The console has them: they are kept no longer.
                    let mut kept = shared.results.lock().expect("results");
                    kept.retain(|result| !results.iter().any(|said| said.id == result.id));
                    self.keep(&kept);
                }
                self.answered(answer).await
            }
            Err(error) => {
                // What was done for the console is said again with the next report.
                match error {
                    cf_console_client::Error::Unauthorized => {
                        tracing::error!("the console does not know this host's token; enrol it again");
                        shared.config.report_every * 5
                    }
                    error => {
                        tracing::warn!(%error, waiting = console.outbox.len(), "the console could not be told");
                        shared.config.report_every
                    }
                }
            }
        }
    }

    /// Whether a job event was queued since the last report, for a console that took that one. A console that
    /// did not is told in its turn, not once for every event.
    fn has_news(&self) -> bool {
        let Some(console) = &self.shared.console else {
            return false;
        };
        let (queued, taken) = *self.shared.told.lock().expect("told");
        taken && console.outbox.queued() != queued
    }

    async fn answered(&self, answer: Answer) -> Duration {
        let shared = &self.shared;
        *shared.console_paused.lock().expect("paused") = answer.paused;
        // The console sends a command until it has its result, and stops for a test run that ended or was cancelled
        // there: one that waits for room and is not in this answer is not wanted any more.
        if let Some((id, wanted)) = shared.test_run.lock().expect("test run").as_ref()
            && !answer.commands.iter().any(|command| &command.id == id)
        {
            wanted.store(false, Ordering::SeqCst);
        }
        shared.machines.pause(answer.paused);
        for command in answer.commands {
            // A result names its command by the id. One the console would not read back cannot be answered, and a
            // result it refuses would hold up every report after it: such a command is not done at all.
            if command.id.is_empty() || command.id.encode_utf16().count() > cf_console_client::RESULT_ID_MAX {
                tracing::warn!(kind = %cut(command.kind, 50), "a command of the console has no id a result can name");
                continue;
            }
            // Under way first: a command's result is in before it is no longer under way, so a command that was
            // taken is one or the other whenever the update ends meanwhile.
            let under_way = shared.update.lock().expect("update").command.as_deref() == Some(&command.id)
                || shared
                    .test_run
                    .lock()
                    .expect("test run")
                    .as_ref()
                    .is_some_and(|(id, _)| id == &command.id);
            let (done, full) = {
                let results = shared.results.lock().expect("results");
                (
                    results.iter().any(|result| result.id == command.id),
                    results.len() >= cf_console_client::MAX_RESULTS,
                )
            };
            if done || under_way || self.restart_requested() {
                continue;
            }
            // Taken only while its result fits in the next report: the console sends the command again with the
            // answer to that report, which takes the results that wait.
            if full {
                tracing::info!(id = %command.id, "a command of the console waits for room for its result");
                continue;
            }
            tracing::info!(id = %command.id, kind = %command.kind, "a command of the console");
            // An update says what came of it when it is over.
            let Some((ok, message)) = self.command(&command).await else {
                continue;
            };
            self.push_result(command.id, ok, message);
            if command.kind == "restart" && ok {
                // Said and kept first: the Runner that starts reports it, and does not restart again.
                self.restart();
            }
        }
        Duration::from_secs(answer.report_every.clamp(1, 600))
    }

    /// Does one command of the console's, and says what came of it; an update says so when it is over. Only what
    /// this Runner can do today is done; the rest is answered as not done, so that the console's page says so
    /// instead of waiting.
    async fn command(&self, command: &Command) -> Option<(bool, String)> {
        Some(match command.kind.as_str() {
            "restart" if self.update_under_way() => (
                false,
                "an update is under way; it starts the Runner again itself".into(),
            ),
            "restart" if self.shared.applying.lock().expect("applying").under_way => (
                false,
                "the Runner waits for its machines to end to start again on new settings; it starts again itself"
                    .into(),
            ),
            "restart" => (
                true,
                format!(
                    "{} ends and its service manager starts it again",
                    self.shared.config.version
                ),
            ),
            "update" => return self.update(&command.id, Ok(None)),
            "update-to" => {
                return self.update(&command.id, commit_from_args(command.args.as_ref()).map(Some));
            }
            "stop-runner" => {
                let name = command
                    .args
                    .as_ref()
                    .and_then(|args| args.get("name").or_else(|| args.get("runner")))
                    .and_then(|name| name.as_str());
                let Some(name) = name else {
                    return Some((false, "the command names no Actions runner".into()));
                };
                let stands = self
                    .shared
                    .machines
                    .snapshot()
                    .machines
                    .iter()
                    .any(|machine| machine.runner == name);
                if !stands {
                    return Some((false, format!("{name} is not on this host")));
                }
                self.shared.machines.stop(crate::names::job_name(name));
                (true, format!("{name} is being stopped"))
            }
            "settings" => return Some(self.change_settings(command)),
            "test-run" => return self.test_run(command),
            kind => (false, format!("this Runner does not do `{kind}` yet")),
        })
    }

    /// The kinds of command this Runner does, for the console to offer no other. `pause` and `resume` are the
    /// console's `paused` in each answer, which every Runner on every system takes: no new job, running ones finish.
    fn commands(&self) -> Vec<String> {
        let shared = &self.shared;
        let mut kinds = vec!["pause", "resume", "restart", "stop-runner"];
        if shared.updates.is_some() {
            kinds.extend(["update", "update-to"]);
        }
        if shared.settings.is_some() {
            kinds.push("settings");
        }
        if shared.console.is_some() {
            kinds.push("test-run");
        }
        kinds.into_iter().map(str::to_string).collect()
    }

    /// Starts a test run's machine ([`crate::test_run`]), or says at once why none starts. One that waits for room says
    /// what came of it when its wait is over: the report loop never waits with it.
    fn test_run(&self, command: &Command) -> Option<(bool, String)> {
        let shared = &self.shared;
        let console = shared
            .console
            .as_ref()
            .map_or("", |console| console.client.base());
        let run = match TestRun::from_args(command.args.as_ref(), console) {
            Ok(run) => run,
            Err(why) => return Some((false, why)),
        };
        let runner = run.runner();
        // The console sends a command again until it has its result: a Runner that died between the start and the
        // result finds the machine it started, even once a check since refuses Docker.
        if shared.machines.knows(&runner) {
            return Some((true, format!("started {runner}")));
        }
        if run.docker
            && let Some(why) = self.docker_refused()
        {
            // The console hands the run on to another machine.
            return Some((false, format!("never: {why}")));
        }
        let busy = if self.update_under_way() {
            Some("an update is under way".to_string())
        } else if shared.applying.lock().expect("applying").under_way {
            Some("the Runner waits to start again on new settings".to_string())
        } else {
            shared
                .test_run
                .lock()
                .expect("test run")
                .is_some()
                .then(|| "a test run waits for room on this host".to_string())
        };
        if let Some(why) = busy {
            return Some((false, format!("busy: {why}")));
        }
        if let Err(why) = run.timeout_within(shared.machines.deadline()) {
            return Some((false, format!("never: {why}")));
        }
        let other = shared.machines.test_run_on();
        match test_run::admit(&*shared.machines, &run, other.as_deref()) {
            Admission::Start(ticket) => Some(self.start_test_run(ticket, &run)),
            Admission::Busy(why) => Some((false, format!("busy: {why}"))),
            Admission::Never(why) => Some((false, format!("never: {why}"))),
            Admission::Waits(ticket) => {
                tracing::info!(%runner, wait_s = run.wait_s, "a test run waits for room");
                let wanted = Arc::new(AtomicBool::new(true));
                *shared.test_run.lock().expect("test run") = Some((command.id.clone(), wanted.clone()));
                let service = self.handle();
                let id = command.id.clone();
                tokio::spawn(async move {
                    let wait = Duration::from_secs(run.wait_s);
                    let waited = test_run::wait_turn(&*service.shared.machines, ticket, wait, || {
                        wanted.load(Ordering::SeqCst)
                    })
                    .await;
                    let result = match waited {
                        Waited::Ready => Some(service.start_test_run(ticket, &run)),
                        Waited::RanOut => Some((false, format!("busy: no room within {}s", run.wait_s))),
                        // The console asks no more: nothing is said to it of this run.
                        Waited::Withdrawn => {
                            tracing::info!(%id, "the console no longer sends a test run that waited; its place is given back");
                            None
                        }
                    };
                    // The result is in before the run no longer waits, as `answered` reads them.
                    let mut waiting = service.shared.test_run.lock().expect("test run");
                    if let Some((ok, message)) = result {
                        service.push_result(id, ok, message);
                    }
                    *waiting = None;
                });
                None
            }
        }
    }

    /// Why nothing Docker comes to this host: its last Docker check failed. None while it passed, and before the
    /// first one ended: Docker is unknown then, not refused.
    fn docker_refused(&self) -> Option<String> {
        self.shared.machines.docker_refusal()
    }

    /// Boots the Docker check's machine when one is due ([`crate::docker_check`]): at the start, once the budget gives
    /// it a slot, and every six hours while it fails. Not while one stands, nor while an update or new settings wait
    /// to start the Runner again; the budget refuses its slot while the host is paused or draining. When the check's
    /// answer changes what the host serves, the label sets are read again.
    pub fn look_at_docker(&self) {
        let shared = &self.shared;
        let machines = &shared.machines;
        let refused = self.docker_refused().is_some();
        if shared.docker_refused.swap(refused, Ordering::SeqCst) != refused {
            shared.rediscover.notify_one();
        }
        if machines.probing()
            || self.restart_requested()
            || self.update_under_way()
            || shared.applying.lock().expect("applying").under_way
            || !machines.docker_check_due(secs(shared.clock.now()))
        {
            return;
        }
        let settings = machines.settings();
        let Some(size) = Size::ALL.into_iter().find(|&size| settings.can_run(size).is_ok()) else {
            return;
        };
        if let Err(why) = machines.start_docker_probe(size) {
            tracing::debug!(%why, "the Docker check waits for a slot");
        }
    }

    /// Starts a test run's machine on its slot, which goes back when none starts.
    fn start_test_run(&self, ticket: crate::traits::Ticket, run: &TestRun) -> (bool, String) {
        let machines = &self.shared.machines;
        // A run that waited for room may find that a check failed meanwhile.
        if run.docker
            && let Some(why) = self.docker_refused()
        {
            machines.release(ticket);
            return (false, format!("never: {why}"));
        }
        match machines.start_test_run(ticket, run) {
            Ok(runner) => (true, format!("started {runner}")),
            Err(error) => {
                machines.release(ticket);
                match error {
                    test_run::StartError::Closed => (false, format!("busy: {error}")),
                    test_run::StartError::Failed(_) => (false, error.to_string()),
                }
            }
        }
    }

    /// Writes the console's new settings into the settings file, which the Runner then takes up as it does any
    /// change of the file: no new job until the machines that stand are gone, then it starts again on them.
    fn change_settings(&self, command: &Command) -> (bool, String) {
        let Some(file) = self.shared.settings.clone() else {
            return (false, "this Runner has no settings file to write them to".into());
        };
        if self.update_under_way() {
            return (
                false,
                "an update is under way; send the settings once it is over".into(),
            );
        }
        let change = match SettingsChange::from_args(command.args.as_ref()) {
            Ok(change) => change,
            Err(why) => return (false, why),
        };
        match file.change(&change) {
            Err(why) => (false, why),
            Ok(false) => (true, "the Runner runs with these settings already".into()),
            Ok(true) => {
                tracing::info!(?change, "the console's new settings are in the settings file");
                self.look_at_settings();
                (
                    true,
                    "the new settings are in the settings file; the Runner starts again on them once no machine stands"
                        .into(),
                )
            }
        }
    }

    /// The limits the `settings` command changes, as they are now; none without a settings file to change them in.
    fn limits(&self, settings: &cf_budget::Settings) -> Option<JobLimits> {
        self.shared.settings.as_ref()?;
        let floor = settings.away.floor_bytes as f64 / settings.total_bytes.max(1) as f64 * 100.0;
        Some(JobLimits {
            max_jobs: settings.max_machines,
            idle_minutes: (settings.mode == cf_budget::Mode::Workstation)
                .then(|| u32::try_from(settings.idle_after.0 / 60_000).unwrap_or(u32::MAX)),
            min_free_memory_percent: (floor * 100.0).round() / 100.0,
            // Not rounded: hours that come back as they went are whole minutes again.
            max_job_hours: self.shared.machines.deadline().as_secs_f64() / 3600.0,
        })
    }

    /// Starts an update in the background, to the head of the branch or to the commit asked for, or says at once
    /// why none starts, also when the command asks for no commit the Runner takes.
    fn update(&self, id: &str, commit: Result<Option<String>, String>) -> Option<(bool, String)> {
        let Some(updates) = self.shared.updates.clone() else {
            return Some((false, "this Runner has nowhere to update from".into()));
        };
        let commit = match commit {
            Ok(commit) => commit,
            Err(why) => return Some((false, why)),
        };
        if self.shared.applying.lock().expect("applying").under_way {
            return Some((
                false,
                "the Runner waits to start again on new settings; update once it has".into(),
            ));
        }
        {
            let mut update = self.shared.update.lock().expect("update");
            update.asked_at = Some(secs(self.shared.clock.now()));
            if update.command.is_some() {
                return Some((false, "an update is under way already".into()));
            }
            update.command = Some(id.to_string());
            update.doing = Some(match &commit {
                Some(commit) => format!("updating to {commit}: building"),
                None => "updating: building".into(),
            });
        }
        let service = self.handle();
        let id = id.to_string();
        tokio::spawn(async move {
            let outcome = service.updating(&id, updates, commit).await;
            let mut update = service.shared.update.lock().expect("update");
            update.doing = None;
            match outcome {
                // Installed: the Runner that starts next says how it went.
                Ok(None) => {}
                // The result is in before the command is no longer under way, as `answered` reads them.
                Ok(Some(message)) => {
                    service.push_result(id, true, message);
                    update.command = None;
                    update.error = None;
                    update.failures = 0;
                }
                Err(message) => {
                    tracing::warn!(%message, "the update did not take; the Runner goes on as it was");
                    service.push_result(id, false, message.clone());
                    // Lifted before the update is no longer under way: new settings that waited for it may hold next.
                    service.shared.machines.hold_for_restart(false);
                    update.command = None;
                    update.error = Some((message, secs(service.shared.clock.now())));
                    update.failures = update.failures.saturating_add(1);
                }
            }
        });
        None
    }

    /// Builds, waits for the machines that stand to end, installs and ends the Runner. Answers what to tell the
    /// console when it is over without an install, `None` once the new program is in place.
    async fn updating(
        &self,
        id: &str,
        updates: Arc<dyn SelfUpdate>,
        commit: Option<String>,
    ) -> Result<Option<String>, String> {
        let shared = &self.shared;
        let began = Instant::now();
        let running = shared.config.version.clone();
        let asked_for_one = commit.is_some();
        let built = {
            let updates = updates.clone();
            tokio::task::spawn_blocking(move || updates.build(commit.as_deref()))
                .await
                .map_err(|error| format!("the build stopped: {error}"))??
        };
        if built == running {
            return Ok(Some(if asked_for_one {
                format!("{running} runs already: it is the commit asked for")
            } else {
                format!("{running} runs already: there is nothing newer")
            }));
        }
        tracing::info!(from = %running, to = %built, "an update is built; no new job until the machines that stand are gone");
        self.drained(&format!("updating to {built}"), began)
            .await
            .map_err(|waited| format!("{built} was not installed: {waited}"))?;
        shared.update.lock().expect("update").doing = Some(format!("updating to {built}: installing"));
        let pending = Pending {
            id: id.to_string(),
            from: running,
            to: built,
            starts: 0,
            failed: None,
        };
        tokio::task::spawn_blocking(move || updates.install(&pending))
            .await
            .map_err(|error| format!("the install stopped: {error}"))??;
        self.restart();
        Ok(None)
    }

    /// Takes no new job and waits for the machines that stand, and the jobs promised a slot, to be gone; the
    /// console's page says what it waits for, after `doing`. An error says what was still waited for when
    /// [`ServiceConfig::update_limit`] after `began` had passed. The hold stays: whoever asked lifts it.
    async fn drained(&self, doing: &str, began: Instant) -> Result<(), String> {
        let shared = &self.shared;
        shared.machines.hold_for_restart(true);
        loop {
            // A job promised a slot before the hold may still get a machine: it is waited for too, until it comes
            // or the promise runs out.
            let (standing, coming) = (shared.machines.unsettled(), shared.machines.coming());
            if standing + coming == 0 {
                break;
            }
            let plural = |n: usize, one: &str, more: &str| format!("{n} {}", if n == 1 { one } else { more });
            let machines = plural(standing, "machine to end", "machines to end");
            let jobs = plural(coming, "job that may still come", "jobs that may still come");
            let waits_for = match (standing, coming) {
                (_, 0) => machines,
                (0, _) => jobs,
                _ => format!("{machines} and {jobs}"),
            };
            if began.elapsed() >= shared.config.update_limit {
                return Err(format!(
                    "still waiting for {waits_for} after {}",
                    crate::update::spoken(shared.config.update_limit),
                ));
            }
            shared.update.lock().expect("update").doing = Some(format!("{doing}: waiting for {waits_for}"));
            tokio::time::sleep(shared.config.tick_every).await;
        }
        Ok(())
    }

    /// Looks at the settings file, and takes new settings up: no new job until the machines that stand are gone,
    /// then the Runner ends so that its service manager starts it again on them. Settings no Runner could start on
    /// are said once and change nothing; a wait that runs out gives up on that version of the file. Not while an
    /// update is under way, which starts the Runner again itself.
    pub fn look_at_settings(&self) {
        let Some(settings) = self.shared.settings.clone() else {
            return;
        };
        if self.restart_requested() || self.update_under_way() {
            return;
        }
        let mut applying = self.shared.applying.lock().expect("applying");
        if applying.under_way {
            return;
        }
        match settings.look() {
            SettingsLook::Same => *applying = Applying::default(),
            SettingsLook::Unusable { why } => {
                if applying.told.as_ref() != Some(&why) {
                    tracing::warn!(%why, "the settings file changed and no Runner could start on it; this one goes on as it is");
                    applying.warning = Some((unusable(&why), secs(self.shared.clock.now())));
                    applying.told = Some(why);
                }
            }
            SettingsLook::Changed { id } => {
                if applying.gave_up.as_ref() == Some(&id) {
                    return;
                }
                applying.under_way = true;
                applying.told = None;
                applying.warning = None;
                drop(applying);
                tracing::info!(
                    "new settings; no new job until the machines that stand are gone, then the Runner starts again on them"
                );
                let service = self.handle();
                tokio::spawn(async move { service.apply_settings(id, settings).await });
            }
        }
    }

    async fn apply_settings(&self, id: String, settings: Arc<dyn SettingsFile>) {
        let shared = &self.shared;
        let waited = self.drained("applying new settings", Instant::now()).await;
        // The file once more: what the next Runner reads is what it is now.
        let now = secs(shared.clock.now());
        let left = match (waited, settings.look()) {
            (Ok(()), SettingsLook::Changed { .. }) => {
                tracing::info!("the machines are gone; the Runner starts again on the new settings");
                self.restart();
                return;
            }
            (Ok(()), SettingsLook::Same) => {
                tracing::info!("the settings are as the Runner started with again; it goes on");
                Applying::default()
            }
            (Ok(()), SettingsLook::Unusable { why }) => {
                tracing::warn!(%why, "the settings file changed again and no Runner could start on it; this one goes on as it is");
                Applying {
                    warning: Some((unusable(&why), now)),
                    told: Some(why),
                    ..Applying::default()
                }
            }
            (Err(waited), _) => {
                tracing::warn!(%waited, "new settings were not taken up; the Runner goes on with the ones it has until the file changes again");
                Applying {
                    gave_up: Some(id),
                    warning: Some((format!("new settings were not taken up: {waited}"), now)),
                    ..Applying::default()
                }
            }
        };
        shared.update.lock().expect("update").doing = None;
        shared.machines.hold_for_restart(false);
        *shared.applying.lock().expect("applying") = left;
    }

    /// Runs the service until `stop` is set: the machines' tick every second, a look at GitHub's repositories now
    /// and then, the sweep of registrations, the reports. Then it stops taking jobs and closes its sessions; the
    /// machines that stand go on and are found by the Runner that starts next.
    pub async fn run(&self, mut stop: Shutdown) {
        let config = &self.shared.config;
        let machines = self.shared.machines.clone();
        let (tick_stop, mut ticking) = cf_scaleset::shutdown();
        let every = config.tick_every;
        let ticks = tokio::spawn(async move {
            let mut timer = tokio::time::interval(every);
            timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = ticking.wait() => return,
                    _ = timer.tick() => {}
                }
                // A look at every vm-host: it may wait on a socket, so it has a thread of its own.
                let machines = machines.clone();
                if let Err(error) = tokio::task::spawn_blocking(move || machines.tick()).await {
                    tracing::error!(%error, "a tick failed");
                }
            }
        });
        // The looks at GitHub go on beside the loop: one that waits on GitHub holds up no report and no stop.
        let (looks_stop, looking) = cf_scaleset::shutdown();
        let looks = tokio::spawn(self.handle().look_now_and_then(looking));
        let mut sweep = tokio::time::interval(config.sweep_every);
        let mut settings = tokio::time::interval(config.settings_every);
        settings.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut report = Box::pin(tokio::time::sleep(Duration::ZERO));
        let mut news = tokio::time::interval(config.tick_every);
        news.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = stop.wait() => break,
                () = self.shared.restarting.notified() => {
                    tracing::info!("the Runner ends to start again");
                    break;
                }
                _ = sweep.tick() => self.sweep().await,
                _ = settings.tick() => self.look_at_settings(),
                () = &mut report => {
                    let next = self.report().await;
                    report.as_mut().reset(tokio::time::Instant::now() + next);
                }
                // A job that was queued, started or ended is on the console's page within a moment, not with
                // the next report in its turn.
                _ = news.tick() => {
                    self.look_at_docker();
                    self.watch_stall(&self.sets(), &self.shared.machines.snapshot());
                    if self.has_news() {
                        let next = self.report().await;
                        report.as_mut().reset(tokio::time::Instant::now() + next);
                    }
                }
            }
        }
        looks_stop.trigger();
        if let Err(error) = looks.await {
            tracing::error!(%error, "the looks at GitHub failed");
        }
        self.shutdown().await;
        tick_stop.trigger();
        let _ = ticks.await;
    }

    /// [`Service::discover`] every [`ServiceConfig::discover_every`] and [`Service::look_at_queue`] every
    /// [`ServiceConfig::queue_every`], one at a time, until `stop`. A look under way when it comes is given up: the
    /// shutdown that follows closes every scale set anyway.
    async fn look_now_and_then(self, mut stop: Shutdown) {
        let config = &self.shared.config;
        let mut discover = tokio::time::interval(config.discover_every);
        let mut queue = tokio::time::interval(config.queue_every);
        queue.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let looks = async {
            loop {
                tokio::select! {
                    _ = discover.tick() => {
                        self.discover().await;
                        // A discovery reads the queue too: the next look at it is a whole interval away.
                        queue.reset();
                    }
                    () = self.shared.rediscover.notified() => {
                        self.discover().await;
                        queue.reset();
                    }
                    _ = queue.tick() => self.look_at_queue().await,
                }
            }
        };
        tokio::select! {
            () = stop.wait() => {}
            () = looks => {}
        }
    }

    /// Closes every scale set's session at GitHub, one after the other.
    async fn close_all(&self) {
        let names: Vec<String> = self
            .shared
            .serving
            .lock()
            .expect("serving")
            .keys()
            .cloned()
            .collect();
        for name in names {
            self.close(&name).await;
        }
    }

    /// Stops taking jobs, closes every session at GitHub and tells the console once more. Machines are left as
    /// they are.
    pub async fn shutdown(&self) {
        self.shared.machines.drain();
        self.shared.ending.store(true, Ordering::SeqCst);
        self.close_all().await;
        self.sweep().await;
        if self.shared.console.is_some() {
            let _ = tokio::time::timeout(Duration::from_secs(10), self.report()).await;
        }
    }
}

/// One scale set's listener, for as long as it is wanted: the scale set is made, a session opened, and the scaler
/// hears its messages; a session that fails is opened again after a wait.
async fn serve<H: Host + Send + 'static, V: Vms + 'static>(
    shared: Arc<Shared<H, V>>,
    client: ScaleSetClient,
    state: Arc<Mutex<SetState>>,
    stop: Shutdown,
) {
    let bell = shared.nudge.subscribe();
    let (name, labels, size, docker, repo) = {
        let state = state.lock().expect("set");
        (
            state.name.clone(),
            state.labels.clone(),
            state.size,
            state.docker,
            state.repo.clone(),
        )
    };
    let (first, longest, healthy) = shared.config.backoff;
    let session_stop = stop.clone();
    supervise(
        stop,
        Backoff::new(first, longest, healthy),
        || {
            let (shared, client, state, bell) = (shared.clone(), client.clone(), state.clone(), bell.clone());
            let (name, labels, repo, stop) = (name.clone(), labels.clone(), repo.clone(), session_stop.clone());
            async move {
                let set = client.ensure_scale_set(&name, &labels).await?;
                state.lock().expect("set").id = set.id;
                tracing::info!(
                    set = %name, id = set.id, %repo, labels = %labels.join(","), size = size.label(),
                    "a scale set at GitHub"
                );
                let registry = SetRegistry::of_set(client.clone(), set.id);
                let machines = shared.machines.clone();
                reconcile(&registry, &shared.roster, &repo, set.id, |runner, answer| {
                    machines.answered(runner, answer);
                })
                .await;
                let session = client.open_session(set.id).await?;
                tracing::info!(set = %name, "a session is open: the Runner listens for this scale set's jobs");
                let slots: Arc<dyn Slots> = machines.clone();
                let mut scaler = Scaler::new(
                    ScalerConfig {
                        scale_set_id: set.id,
                        repo,
                        machine: shared.config.machine.clone(),
                        size,
                        docker,
                    },
                    session.clone(),
                    registry,
                    machines.clone(),
                    slots,
                    shared.roster.clone(),
                    shared.clock.clone(),
                )
                .with_events(machines.clone())
                .with_out_of_memory_check({
                    let machines = machines.clone();
                    Arc::new(move |runner| machines.was_out_of_memory(runner))
                });
                let (statistics, never) = (scaler.statistics_handle(), scaler.never_handle());
                {
                    let mut state = state.lock().expect("set");
                    state.error = None;
                    state.error_at = None;
                }
                let watch = {
                    let (state, clock) = (state.clone(), shared.clock.clone());
                    move |capacity: Option<usize>| {
                        let mut state = state.lock().expect("set");
                        state.statistics.clone_from(&statistics.lock().expect("statistics"));
                        state.never.clone_from(&never.lock().expect("never"));
                        // The room the next poll offers GitHub beyond the jobs it counts as assigned here.
                        let assigned = state.statistics.as_ref().map_or(0, Statistics::assigned);
                        state.room_since = match capacity {
                            Some(capacity) if capacity > assigned => state.room_since.or_else(|| Some(clock.now())),
                            _ => None,
                        };
                    }
                };
                let listened = listen_nudged(&session, &mut Watched(&mut scaler, &watch), stop, &bell).await;
                drop(scaler);
                // No poll offers room until the next session's first.
                state.lock().expect("set").room_since = None;
                match session.close().await {
                    Ok(()) => tracing::info!(set = %name, "the session is closed"),
                    Err(error) => {
                        tracing::info!(set = %name, %error, "the session could not be closed; it expires by itself");
                    }
                }
                listened
            }
        },
        |error, wait| {
            tracing::warn!(set = %name, %error, wait = ?wait, "a listener failed; it starts again");
            let mut state = state.lock().expect("set");
            state.error = Some(error.to_string());
            state.error_at = Some(secs(shared.clock.now()));
        },
    )
    .await;
}

/// A scaler whose counts, and the room it offers GitHub next (none after a failure), are copied to the scale set's
/// state after every message.
struct Watched<'a, S, F>(&'a mut S, &'a F);

impl<S: cf_scaleset::Scaler, F: Fn(Option<usize>) + Send + Sync> cf_scaleset::Scaler for Watched<'_, S, F> {
    async fn scale(&mut self, message: Option<&cf_scaleset::Message>) -> Result<usize, cf_scaleset::Error> {
        let capacity = self.0.scale(message).await;
        (self.1)(capacity.as_ref().ok().copied());
        capacity
    }
}

/// The key file a host still holds where its App's key was, as the report names it: an absolute path of at most 1024
/// bytes that is there (a link included, which is not followed), or none.
fn held_key_file(path: Option<&Path>) -> Option<String> {
    let path = path?;
    let text = path.to_str()?;
    if !path.is_absolute() || text.len() > 1024 {
        return None;
    }
    std::fs::symlink_metadata(path).ok().map(|_| text.to_string())
}

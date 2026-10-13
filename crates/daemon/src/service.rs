//! The Runner's service: it finds the repositories and sizes to serve, keeps one listener per scale set, lets the
//! machines see the time pass, removes the registrations at GitHub that have to go, and reports to the console.
//!
//! Every round is a method of its own ([`Service::discover`], [`Service::sweep`], [`Service::report`]) so that a
//! test drives them one at a time; [`Service::run`] is the loop that calls them until it is told to stop.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use cf_budget::{Host, Pressure, Size};
use cf_console_client::{
    Activity, Answer, Client, Command, CommandResult, HostStatus, JobKind, MAX_RUNNERS, MAX_SCALE_SETS,
    MachineInfo, Outbox, Power, Report, RunnerJob, RunnerStatus, RunnerUsage, ScaleSetStatus, Status, Update,
};
use cf_ghapp::{Clock, GitHubApp};
use cf_host_macos::Battery;
use cf_scaleset::{Backoff, ScaleSetClient, Shutdown, ShutdownTrigger, Statistics, listen_nudged, supervise};
use cf_workflows::self_hosted_with;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

use crate::adapters::SetRegistry;
use crate::machines::{self, JobReport, Machines, Reports, Snapshot};
use crate::names::{ServePolicy, scale_set_name};
use crate::roster::Roster;
use crate::scaler::{Scaler, ScalerConfig, reconcile};
use crate::secs;
use crate::traits::{EventKind, Slots};
use crate::update::{Pending, SelfUpdate};
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
    /// stand included. Under the hour the console waits for a command's result.
    pub update_limit: Duration,
}

impl ServiceConfig {
    /// The times the Runner keeps in service.
    pub fn new(machine: &str, hostname: &str, model: &str, version: &str) -> ServiceConfig {
        ServiceConfig {
            machine: machine.to_string(),
            hostname: hostname.to_string(),
            model: model.to_string(),
            version: version.to_string(),
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
    /// Why its listener is not listening, when it is not.
    pub error: Option<String>,
    /// Since when, in seconds since 1970.
    pub error_at: Option<i64>,
    /// GitHub's counts, as the listener last heard them.
    pub statistics: Option<Statistics>,
    /// Why a job assigned to it can never start on this Mac, when one cannot.
    pub never: Option<String>,
}

/// One look's reading of GitHub.
struct Read {
    /// The scale sets to serve, by name.
    wanted: BTreeMap<String, Wanted>,
    /// Repositories that could not be read: nothing is concluded about them.
    unread: BTreeSet<String>,
    problems: Vec<String>,
    /// How many repositories this Mac serves.
    served: usize,
}

struct Wanted {
    repo: String,
    client: ScaleSetClient,
    labels: Vec<String>,
    size: Size,
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
    results: Mutex<Vec<CommandResult>>,
    /// Registrations GitHub would not remove, and when each is tried again.
    refused: Mutex<BTreeMap<String, Instant>>,
    console_paused: Mutex<bool>,
    /// The outbox's count of queued events as of the last report, and whether the console took that report: news
    /// goes to a console that answers at once, and to one that does not with the next report in its turn.
    told: Mutex<(u64, bool)>,
    /// Jobs that wait while the Mac could run them, since the first look that saw them.
    stall: Mutex<Option<Stall>>,
    updates: Option<Arc<dyn SelfUpdate>>,
    results_file: Option<PathBuf>,
    update: Mutex<Updating>,
    /// Set when the Runner is to end so that launchd starts it again: a restart, or an update installed.
    restart: AtomicBool,
    restarting: Notify,
    /// Set while the Runner ends: the last report says so instead of serving nothing.
    ending: AtomicBool,
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
        }),
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

impl<H: Host + Send + 'static, V: Vms + 'static> Service<H, V> {
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
                refused: Mutex::new(BTreeMap::new()),
                console_paused: Mutex::new(false),
                told: Mutex::new((0, false)),
                stall: Mutex::new(None),
                updates: parts.updates,
                update: Mutex::new(Updating::default()),
                restart: AtomicBool::new(false),
                restarting: Notify::new(),
                ending: AtomicBool::new(false),
                results: Mutex::new(kept_results(parts.results.as_deref())),
                results_file: parts.results,
            }),
        }
    }

    /// Says what came of a command that is not one of a report's answer: an update the Runner that starts finishes.
    /// Kept until the console has it.
    pub fn said(&self, id: &str, ok: bool, message: &str) {
        let shared = &self.shared;
        {
            let mut update = shared.update.lock().expect("update");
            if update.command.as_deref() == Some(id) {
                update.command = None;
            }
        }
        self.push_result(id.to_string(), ok, message.to_string());
    }

    /// Takes on an update a Runner before installed, which this one finishes: its command is not done again
    /// meanwhile.
    pub fn take_up(&self, pending: &Pending) {
        self.shared.update.lock().expect("update").command = Some(pending.id.clone());
    }

    /// Whether the Runner is to end so that launchd starts it again.
    pub fn restart_requested(&self) -> bool {
        self.shared.restart.load(Ordering::SeqCst)
    }

    /// Ends [`Service::run`] so that launchd starts the Runner again.
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
        if results.len() < cf_console_client::MAX_RESULTS {
            results.push(CommandResult {
                id,
                ok,
                message: cut(message, STATUS_TEXT_MAX),
                at: iso(secs(shared.clock.now())),
            });
        }
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

    /// One look at GitHub. `wants`: whether this Mac still wants scale sets; when not, every one of its own goes.
    /// Says which scale sets of this Mac it could not delete.
    async fn look(&self, wants: bool) -> Vec<String> {
        let shared = &self.shared;
        let machine = &shared.config.machine;
        let Read {
            wanted,
            unread,
            problems,
            served,
        } = match self.read(wants).await {
            Ok(read) => read,
            Err(error) => {
                // Nothing is known: everything stays as it is.
                tracing::warn!(%error, "the App's installations could not be read");
                *shared.discovery_error.lock().expect("discovery") = Some(format!("GitHub: {error}"));
                return vec!["GitHub could not be asked for this Mac's scale sets".into()];
            }
        };

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
            if unread.contains(&repo) {
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
                self.close(&set.name).await;
                if shared.roster.active(&repo, set.id) == 0 {
                    match client.delete_scale_set(set.id).await {
                        Ok(()) => {
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
                error: None,
                error_at: None,
                statistics: None,
                never: None,
            }));
            let (stop, stopped) = cf_scaleset::shutdown();
            let task = tokio::spawn(serve(shared.clone(), want.client.clone(), state.clone(), stopped));
            serving.insert(name, Serving { stop, task, state });
        }
        *shared.not_serving.lock().expect("not serving") = (served == 0 && problems.is_empty())
            .then(|| "the App is installed on no repository this Mac serves".to_string());
        *shared.discovery_error.lock().expect("discovery") =
            (!problems.is_empty()).then(|| problems.join("; "));
        for problem in &problems {
            tracing::warn!(%problem, "discovery");
        }
        kept.extend(problems);
        kept
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
        let mut served = 0usize;
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
                served += 1;
                let name = repo.full_name.clone();
                let client = shared
                    .clients
                    .lock()
                    .expect("clients")
                    .entry(name.clone())
                    .or_insert_with(|| {
                        ScaleSetClient::new(
                            shared.app.clone(),
                            installation.id,
                            &name,
                            &repo.html_url,
                            shared.config.poll_timeout,
                        )
                    })
                    .clone();
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
                    let Some(size) = Size::of_labels(set.labels().iter().map(String::as_str)) else {
                        continue;
                    };
                    if let Err(refusal) = settings.can_run(size) {
                        // Not offered to GitHub at all: a job nobody here can run is left to the other Macs.
                        tracing::info!(repo = %name, size = size.label(), why = refusal.name(), "a size this Mac never runs is not served");
                        continue;
                    }
                    wanted.insert(
                        scale_set_name(machine, &name, &set),
                        Wanted {
                            repo: name.clone(),
                            client: client.clone(),
                            labels: set.labels().to_vec(),
                            size,
                        },
                    );
                }
            }
        }
        Ok(Read {
            wanted,
            unread,
            problems,
            served,
        })
    }

    /// What this Mac would serve, read from GitHub and nothing else: no scale set is made, no session opened, no
    /// Actions runner registered. Each line is a repository and the labels of one scale set with its size; the
    /// second list is what could not be read. For a check before the service is installed: a missing permission
    /// of the App (its variables, its workflows) shows here.
    pub async fn survey(&self) -> (Vec<String>, Vec<String>) {
        match self.read(true).await {
            Ok(read) => (
                read.wanted
                    .values()
                    .map(|want| {
                        format!(
                            "{}: [{}] as {}",
                            want.repo,
                            want.labels.join(", "),
                            want.size.label()
                        )
                    })
                    .collect(),
                read.problems,
            ),
            Err(error) => (Vec::new(), vec![format!("GitHub: {error}")]),
        }
    }

    /// The client of a repository no look at GitHub has shown since the Runner started: a registration of before
    /// may be all that is left of it here.
    async fn client_for(&self, repo: &str) -> Option<ScaleSetClient> {
        let shared = &self.shared;
        if let Some(client) = shared.clients.lock().expect("clients").get(repo) {
            return Some(client.clone());
        }
        let installation = shared.app.repo_installation(repo).await.ok()?;
        let found = shared
            .app
            .repositories(installation)
            .await
            .ok()?
            .into_iter()
            .find(|found| found.full_name.eq_ignore_ascii_case(repo))?;
        let client = ScaleSetClient::new(
            shared.app.clone(),
            installation,
            repo,
            &found.html_url,
            shared.config.poll_timeout,
        );
        shared
            .clients
            .lock()
            .expect("clients")
            .insert(repo.to_string(), client.clone());
        Some(client)
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
                left.push(format!("the console was not told that the Mac is gone ({error})"));
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
            let id = match registration.runner_id {
                Some(id) => Ok(Some(id)),
                // Registered or not, nobody heard: GitHub is asked by name.
                None => client
                    .runner_by_name(&registration.runner)
                    .await
                    .map(|found| found.map(|runner| runner.id)),
            };
            // GitHub's answer is what proves anything (`Machines::answered`); no answer (a 5xx, a timeout, a dropped
            // connection) proves nothing, and the removal is sent again at a later sweep.
            let answer = match id {
                Ok(None) => Ok(machines::Answer::Gone),
                Ok(Some(id)) => match client.remove_runner(id).await {
                    Ok(()) => Ok(machines::Answer::Removed),
                    Err(error) if error.is_not_found() => Ok(machines::Answer::Gone),
                    Err(error) if error.status() == Some(422) => Ok(machines::Answer::Refused),
                    Err(error) => Err(error),
                },
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
        let why = if snapshot.open {
            sets.iter()
                .find_map(|set| shared.machines.short_of(set.size))
                .unwrap_or("no machine of their size fits")
                .to_string()
        } else {
            snapshot.reason.clone()
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
                    "the jobs that waited while the Mac was idle are under way or gone"
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
            .and_then(|smallest| shared.machines.short_of(smallest));
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
                // Of the commonest size, under the limits in force.
                max_jobs: settings.side_by_side(&snapshot.budget.limits, Size::G4),
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
                idle_with_jobs_waiting_seconds,
                idle_with_jobs_waiting_reason: idle_with_jobs_waiting_reason
                    .map(|reason| cut(reason, STATUS_TEXT_MAX)),
                scale_sets: sets
                    .iter()
                    .take(MAX_SCALE_SETS)
                    .map(|set| ScaleSetStatus {
                        repo: set.repo.clone(),
                        name: set.name.clone(),
                        id: set.id,
                        labels: set.labels.iter().take(MAX_LABELS).cloned().collect(),
                        capacity: u32::try_from(shared.machines.room(set.size)).unwrap_or(u32::MAX),
                        waiting: set.statistics.as_ref().map_or(0, |s| {
                            u32::try_from(s.total_available_jobs.max(0)).unwrap_or(u32::MAX)
                        }),
                        error: set
                            .error
                            .clone()
                            .or_else(|| {
                                set.never.as_ref().map(|why| {
                                    format!(
                                        "a job of {} can never start on this Mac: {why}",
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
                            // A machine's share of the CPUs is not measured.
                            cpu_percent: -1.0,
                            memory_bytes,
                        }),
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
            settings: None,
            image: None,
            jobs: Vec::new(),
            results: Vec::new(),
        }
    }

    /// Reports to the console, with the job events that wait, and does what its answer says. Returns when the
    /// console wants to hear again. A console that is away loses nothing: the job events wait in the outbox.
    pub async fn report(&self) -> Duration {
        let shared = &self.shared;
        let Some(console) = &shared.console else {
            return shared.config.report_every;
        };
        let mut report = self.status();
        // Kept until the console has them, so that one said meanwhile is written down beside them.
        let results: Vec<CommandResult> = shared.results.lock().expect("results").clone();
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
                        tracing::error!("the console does not know this Mac's token; enrol it again");
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
        shared.machines.pause(answer.paused);
        for command in answer.commands {
            // A result names its command by the id. One the console would not read back cannot be answered, and a
            // result it refuses would hold up every report after it: such a command is not done at all.
            if command.id.is_empty() || command.id.encode_utf16().count() > cf_console_client::RESULT_ID_MAX {
                tracing::warn!(kind = %cut(command.kind, 50), "a command of the console has no id a result can name");
                continue;
            }
            let done = shared
                .results
                .lock()
                .expect("results")
                .iter()
                .any(|result| result.id == command.id);
            let under_way = shared.update.lock().expect("update").command.as_deref() == Some(&command.id);
            if done || under_way || self.restart_requested() {
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
            "restart" if self.shared.update.lock().expect("update").command.is_some() => (
                false,
                "an update is under way; it starts the Runner again itself".into(),
            ),
            "restart" => (
                true,
                format!("{} ends and launchd starts it again", self.shared.config.version),
            ),
            "update" => return self.update(&command.id),
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
                    return Some((false, format!("{name} is not on this Mac")));
                }
                self.shared.machines.stop(crate::names::job_name(name));
                (true, format!("{name} is being stopped"))
            }
            kind => (false, format!("this Runner does not do `{kind}` yet")),
        })
    }

    /// Starts an update in the background, or says at once why none starts.
    fn update(&self, id: &str) -> Option<(bool, String)> {
        let Some(updates) = self.shared.updates.clone() else {
            return Some((false, "this Runner has nowhere to update from".into()));
        };
        {
            let mut update = self.shared.update.lock().expect("update");
            update.asked_at = Some(secs(self.shared.clock.now()));
            if update.command.is_some() {
                return Some((false, "an update is under way already".into()));
            }
            update.command = Some(id.to_string());
            update.doing = Some("updating: building".into());
        }
        let service = Service {
            shared: self.shared.clone(),
        };
        let id = id.to_string();
        tokio::spawn(async move {
            let outcome = service.updating(&id, updates).await;
            let mut update = service.shared.update.lock().expect("update");
            update.doing = None;
            match outcome {
                // Installed: the Runner that starts next says how it went.
                Ok(None) => {}
                Ok(Some(message)) => {
                    update.command = None;
                    update.error = None;
                    update.failures = 0;
                    drop(update);
                    service.push_result(id, true, message);
                }
                Err(message) => {
                    update.command = None;
                    update.error = Some((message.clone(), secs(service.shared.clock.now())));
                    update.failures = update.failures.saturating_add(1);
                    drop(update);
                    service.shared.machines.hold_for_update(false);
                    tracing::warn!(%message, "the update did not take; the Runner goes on as it was");
                    service.push_result(id, false, message);
                }
            }
        });
        None
    }

    /// Builds, waits for the machines that stand to end, installs and ends the Runner. Answers what to tell the
    /// console when it is over without an install, `None` once the new program is in place.
    async fn updating(&self, id: &str, updates: Arc<dyn SelfUpdate>) -> Result<Option<String>, String> {
        let shared = &self.shared;
        let began = Instant::now();
        let running = shared.config.version.clone();
        let built = {
            let updates = updates.clone();
            tokio::task::spawn_blocking(move || updates.build())
                .await
                .map_err(|error| format!("the build stopped: {error}"))??
        };
        if built == running {
            return Ok(Some(format!("{running} runs already: there is nothing newer")));
        }
        tracing::info!(from = %running, to = %built, "an update is built; no new job until the machines that stand are gone");
        shared.machines.hold_for_update(true);
        loop {
            // A slot promised before the hold may still get its job: it is waited for as a machine that stands.
            let standing = shared.machines.unsettled() + shared.machines.promised();
            if standing == 0 {
                break;
            }
            let (machines, stand) = if standing == 1 {
                ("machine", "stands")
            } else {
                ("machines", "stand")
            };
            if began.elapsed() >= shared.config.update_limit {
                return Err(format!(
                    "{built} was not installed: {standing} {machines} still {stand} after {}",
                    crate::update::spoken(shared.config.update_limit),
                ));
            }
            shared.update.lock().expect("update").doing = Some(format!(
                "updating to {built}: waiting for {standing} {machines} to end"
            ));
            tokio::time::sleep(shared.config.tick_every).await;
        }
        shared.update.lock().expect("update").doing = Some(format!("updating to {built}: installing"));
        let pending = Pending {
            id: id.to_string(),
            from: running,
            to: built,
            starts: 0,
        };
        tokio::task::spawn_blocking(move || updates.install(&pending))
            .await
            .map_err(|error| format!("the install stopped: {error}"))??;
        self.restart();
        Ok(None)
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
        let mut discover = tokio::time::interval(config.discover_every);
        let mut sweep = tokio::time::interval(config.sweep_every);
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
                _ = discover.tick() => self.discover().await,
                _ = sweep.tick() => self.sweep().await,
                () = &mut report => {
                    let next = self.report().await;
                    report.as_mut().reset(tokio::time::Instant::now() + next);
                }
                // A job that was queued, started or ended is on the console's page within a moment, not with
                // the next report in its turn.
                _ = news.tick() => {
                    self.watch_stall(&self.sets(), &self.shared.machines.snapshot());
                    if self.has_news() {
                        let next = self.report().await;
                        report.as_mut().reset(tokio::time::Instant::now() + next);
                    }
                }
            }
        }
        self.shutdown().await;
        tick_stop.trigger();
        let _ = ticks.await;
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
    let (name, labels, size, repo) = {
        let state = state.lock().expect("set");
        (
            state.name.clone(),
            state.labels.clone(),
            state.size,
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
                let registry = SetRegistry {
                    client: client.clone(),
                    scale_set_id: set.id,
                };
                let machines = shared.machines.clone();
                reconcile(&registry, &shared.roster, &repo, set.id, |runner, answer| {
                    machines.answered(runner, answer);
                })
                .await;
                let session = client.open_session(set.id, &shared.config.machine).await?;
                tracing::info!(set = %name, "a session is open: the Runner listens for this scale set's jobs");
                let slots: Arc<dyn Slots> = machines.clone();
                let mut scaler = Scaler::new(
                    ScalerConfig {
                        scale_set_id: set.id,
                        repo,
                        machine: shared.config.machine.clone(),
                        size,
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
                    let state = state.clone();
                    move || {
                        let mut state = state.lock().expect("set");
                        state.statistics.clone_from(&statistics.lock().expect("statistics"));
                        state.never.clone_from(&never.lock().expect("never"));
                    }
                };
                let listened = listen_nudged(&session, &mut Watched(&mut scaler, &watch), stop, &bell).await;
                drop(scaler);
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

/// A scaler whose counts are copied to the scale set's state after every message.
struct Watched<'a, S, F>(&'a mut S, &'a F);

impl<S: cf_scaleset::Scaler, F: Fn() + Send + Sync> cf_scaleset::Scaler for Watched<'_, S, F> {
    async fn scale(&mut self, message: Option<&cf_scaleset::Message>) -> Result<usize, cf_scaleset::Error> {
        let capacity = self.0.scale(message).await;
        (self.1)();
        capacity
    }
}

//! The service on a real host: the operating system read for real, vm-hosts started from the Runner's own binary,
//! records and the outbox in the state folder. The binary and the integration suite both build the service here, so
//! what the suite boots is what the service runs.
//!
//! What differs between systems is a [`Platform`]: [`Mac`] on macOS, [`Linux`] on Linux, and [`This`] the one this
//! build runs on. Everything else is the same code on each. Another system is one more implementation of the trait,
//! with nothing above it reshaped.

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod mac;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use cf_budget::{Host, HostError, HostFacts, Mode, Settings};
use cf_console_client::{Client, OUTBOX_CAPACITY, Outbox};
use cf_ghapp::{Clock, GitHubApp, Http, RetryPolicy, SystemClock};
use cf_guest_proto::Workload;
use cf_secret::Secret;

#[cfg(target_os = "linux")]
pub use linux::{Linux, this_user, this_user_bus_env};
#[cfg(target_os = "macos")]
pub use mac::Mac;

use crate::names::ServePolicy;
use crate::update::SelfUpdate;
use crate::{
    Console, DiskStore, HostExtras, LoggedReports, MachineConfig, MachineParts, Machines, Nudge,
    OutboxReports, Reports, Roster, Service, ServiceConfig, ServiceParts, VmHosts,
};

/// The platform this build runs on.
#[cfg(target_os = "macos")]
pub type This = Mac;
/// The platform this build runs on.
#[cfg(target_os = "linux")]
pub type This = Linux;

/// A process as far as the Runner tells one from another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// The program it runs, by its full path.
    pub program: PathBuf,
    /// When it started, in seconds since 1970. A number that went to a later process has a later time.
    pub started_at: u64,
    /// When it started by a clock that no step of the wall clock moves, where the system keeps one: on Linux clock
    /// ticks since the host started. With the host's start, it names the process for good.
    pub mark: Option<u64>,
}

/// What the service needs of the system it runs on, beside what is the same everywhere.
pub trait Platform {
    /// The budget's reader of the host.
    type Host: Host + Send + 'static;
    /// What the system is called in a message: `macOS`, `Linux`.
    const SYSTEM: &'static str;
    /// The file in the state folder's `logs` that the system's service manager writes the Runner's own output to:
    /// what it says when it cannot start, before its log is open.
    const SERVICE_LOG: &'static str;

    /// The host, with the state folder's disk as the one the budget watches. The folder must be there.
    fn host(state_dir: &Path) -> Result<Self::Host, HostError>;

    /// The budget's settings for a host with these facts. `mode` is whether a person works there, on a system where
    /// one does.
    fn settings(facts: HostFacts, mode: Mode) -> Settings;

    /// A name for this start of the host: another one after every start, the same until the next, whatever the
    /// clock does meanwhile. `None` when the system will not say.
    fn boot_session() -> Option<String>;

    /// The host's name.
    fn host_name() -> Option<String>;

    /// The host's model, for the console; a name for the system when the host names none.
    fn model() -> String;

    /// Reads what the console's page shows beside the budget's reading.
    fn extras(state_dir: &Path) -> Result<Box<dyn Fn() -> HostExtras + Send + Sync>, HostError>;

    /// Whatever the system needs done to a state folder the Runner made: its images, scratch disks and machine
    /// files come from a release or are made again. Never fatal: the answer is what to tell the person when it
    /// could not be done.
    fn keep_state(state_dir: &Path) -> Option<String>;

    /// Whether `program`, a copy of the Runner, can start machines on this host; the error says what is missing.
    fn fit(program: &Path) -> Result<(), String>;

    /// The process with this number; `None` once it is gone, or when the system will not say.
    fn process(pid: u32) -> Option<Process>;

    /// The program the process with this number runs; `None` once it is gone. Cheaper than [`Platform::process`],
    /// and no proof the number is still the same process.
    fn program(pid: u32) -> Option<PathBuf>;

    /// The vm-hosts `program` starts in this state folder.
    fn vm_hosts(program: &Path, state_dir: &Path) -> VmHosts;
}

/// The machines of a host of this platform.
pub type MachinesOf<P> = Machines<<P as Platform>::Host, VmHosts>;
/// The service of a host of this platform.
pub type ServiceOf<P> = Service<<P as Platform>::Host, VmHosts>;
/// The machines of a real host.
pub type HostMachines = MachinesOf<This>;
/// The service of a real host.
pub type HostService = ServiceOf<This>;

/// How long the console has to answer one call. A report that fails is not sent again at once: the next one
/// carries what waits, and meanwhile the service's loop goes on.
const CONSOLE_ANSWER: Duration = Duration::from_secs(15);

/// Where the records of machines are kept in a state folder.
pub fn records_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("records")
}

/// What the service on one host is made of.
pub struct HostSetup {
    /// This host's name at GitHub and at the console.
    pub machine: String,
    /// The Runner's version.
    pub version: String,
    /// The folder with everything the Runner keeps. It is made, for this user alone, when it is not there.
    pub state_dir: PathBuf,
    /// The Runner's binary, which every vm-host is started from.
    pub program: PathBuf,
    /// The GitHub App.
    pub app: GitHubApp,
    /// The version of the installed image jobs run in.
    pub image: String,
    /// The longest a job may run.
    pub deadline: Duration,
    /// Whether a person works on the host.
    pub mode: Mode,
    /// Which repositories are served.
    pub policy: ServePolicy,
    /// The console's address and this host's token, when it reports to one.
    pub console: Option<(String, Secret<String>)>,
    /// What runs in a machine in place of GitHub's Actions runner: the integration suite's command. `None` in
    /// service.
    pub stand_in: Option<Workload>,
    /// How the Runner updates itself; none where it cannot.
    pub updates: Option<Arc<dyn SelfUpdate>>,
}

/// Builds the machines and the service of this host, and finds what a Runner before left: records, vm-hosts,
/// waiting job events. Nothing runs yet; [`Service::run`] starts it.
///
/// An error is a host the Runner cannot work on: a state folder that cannot be made, the system not answering what
/// the budget needs, settings no budget takes.
pub fn host_service<P: Platform>(setup: HostSetup) -> Result<(Arc<MachinesOf<P>>, ServiceOf<P>), String> {
    let state = &setup.state_dir;
    let system = |error: HostError| format!("{}: {error}", P::SYSTEM);
    cf_secret::private_dir(state).map_err(|error| format!("{}: {error}", state.display()))?;
    let host = P::host(state).map_err(system)?;
    let facts = host.facts().map_err(system)?;
    // A record of before another start of the host has no machine, so the start must be known.
    let boot =
        P::boot_session().ok_or_else(|| format!("{} does not name this start of the host", P::SYSTEM))?;
    let hostname = P::host_name().unwrap_or_else(|| setup.machine.clone());
    let store = DiskStore::new(records_dir(state))
        .map_err(|error| format!("{}: {error}", records_dir(state).display()))?;
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let roster = Arc::new(Roster::new());
    let bell = Arc::new(Nudge::default());
    let console = setup.console.map(|(address, token)| Console {
        client: Client::new(
            &address,
            token,
            Http::new(
                CONSOLE_ANSWER,
                RetryPolicy {
                    retries: 0,
                    ..RetryPolicy::default()
                },
            ),
        ),
        outbox: Arc::new(Outbox::open(state.join("outbox.json"), OUTBOX_CAPACITY)),
    });
    let reports: Arc<dyn Reports> = match &console {
        Some(console) => Arc::new(OutboxReports(console.outbox.clone())),
        None => Arc::new(LoggedReports),
    };
    let mut config = MachineConfig::new(&setup.machine, &setup.image, &boot, setup.deadline);
    config.stand_in = setup.stand_in;
    config.docker_max_jobs = setup.policy.docker_max_jobs;
    config.docker_cache = (setup.policy.docker_cache_bytes > 0)
        .then(|| (state.join("docker-cache"), setup.policy.docker_cache_bytes));
    let rings = bell.clone();
    let machines = Machines::restore(MachineParts {
        host,
        vms: P::vm_hosts(&setup.program, state),
        settings: P::settings(facts, setup.mode),
        config,
        roster: roster.clone(),
        store: Arc::new(store),
        clock: clock.clone(),
        reports,
        nudge: Box::new(move || rings.fire()),
    })
    .map_err(|error| format!("the budget's settings: {error}"))?;
    let machines = Arc::new(machines);

    let mut service = ServiceConfig::new(&setup.machine, &hostname, &P::model(), &setup.version);
    service.policy = setup.policy;
    let service = Service::new(ServiceParts {
        app: setup.app,
        machines: machines.clone(),
        roster,
        clock,
        nudge: bell,
        console,
        config: service,
        extras: P::extras(state).map_err(system)?,
        updates: setup.updates,
        results: Some(state.join("results.json")),
    });
    Ok((machines, service))
}

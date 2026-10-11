//! The service on a real Mac: macOS read for real, vm-hosts started from the Runner's own binary, records and the
//! outbox in the state folder. The binary and the integration suite both build the service here, so what the
//! suite boots is what the service runs.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_budget::{Host as _, Mode, Settings};
use cf_console_client::{Client, OUTBOX_CAPACITY, Outbox};
use cf_ghapp::{Clock, GitHubApp, Http, RetryPolicy, SystemClock};
use cf_guest_proto::Workload;
use cf_host_macos::{MacHost, battery, boot_session, boot_time, cpu_percent, host_name, load, model};
use cf_secret::Secret;

use crate::names::ServePolicy;
use crate::{
    Console, DiskStore, HostExtras, LoggedReports, MachineConfig, MachineParts, Machines, Nudge,
    OutboxReports, Reports, Roster, Service, ServiceConfig, ServiceParts, VmHosts,
};

/// The machines of a real Mac.
pub type MacMachines = Machines<MacHost, VmHosts>;
/// The service of a real Mac.
pub type MacService = Service<MacHost, VmHosts>;

/// How long the console has to answer one call. A report that fails is not sent again at once: the next one
/// carries what waits, and meanwhile the service's loop goes on.
const CONSOLE_ANSWER: Duration = Duration::from_secs(15);

/// Where the records of machines are kept in a state folder.
pub fn records_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("records")
}

/// What the service on one Mac is made of.
pub struct MacSetup {
    /// This Mac's name at GitHub and at the console.
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
    /// Whether a person works on the Mac.
    pub mode: Mode,
    /// Which repositories are served.
    pub policy: ServePolicy,
    /// The console's address and this Mac's token, when it reports to one.
    pub console: Option<(String, Secret<String>)>,
    /// What runs in a machine in place of GitHub's Actions runner: the integration suite's command. `None` in
    /// service.
    pub stand_in: Option<Workload>,
}

/// Builds the machines and the service of this Mac, and finds what a Runner before left: records, vm-hosts,
/// waiting job events. Nothing runs yet; [`Service::run`] starts it.
///
/// An error is a Mac the Runner cannot work on: a state folder that cannot be made, macOS not answering what the
/// budget needs, settings no budget takes.
pub fn mac_service(setup: MacSetup) -> Result<(Arc<MacMachines>, MacService), String> {
    let state = &setup.state_dir;
    cf_secret::private_dir(state).map_err(|error| format!("{}: {error}", state.display()))?;
    let host = MacHost::new(state).map_err(|error| format!("macOS: {error}"))?;
    let facts = host.facts().map_err(|error| format!("macOS: {error}"))?;
    // A record of before another start of the Mac has no machine, so the start must be known.
    let boot = boot_session().ok_or("macOS does not name this start of the Mac")?;
    let hostname = host_name().unwrap_or_else(|| setup.machine.clone());
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
    let rings = bell.clone();
    let machines = Machines::restore(MachineParts {
        host,
        vms: VmHosts::new(&setup.program, state),
        settings: Settings::for_host(facts, setup.mode),
        config,
        roster: roster.clone(),
        store: Arc::new(store),
        clock: clock.clone(),
        reports,
        nudge: Box::new(move || rings.fire()),
    })
    .map_err(|error| format!("the budget's settings: {error}"))?;
    let machines = Arc::new(machines);

    let mut service = ServiceConfig::new(
        &setup.machine,
        &hostname,
        &model().unwrap_or_else(|| "Mac".into()),
        &setup.version,
    );
    service.policy = setup.policy;
    // A reader of its own for what only the console's page shows; the machines keep theirs.
    let extras = MacHost::new(state).map_err(|error| format!("macOS: {error}"))?;
    let ticks_before = Mutex::new(extras.cpu_ticks());
    let service = Service::new(ServiceParts {
        app: setup.app,
        machines: machines.clone(),
        roster,
        clock,
        nudge: bell,
        console,
        config: service,
        extras: Box::new(move || {
            let now = extras.cpu_ticks();
            let before = std::mem::replace(&mut *ticks_before.lock().expect("ticks"), now);
            HostExtras {
                load: load(),
                disk_total_bytes: extras.disk_total_bytes().unwrap_or(0),
                swap_total_bytes: extras.raw().map_or(0, |raw| raw.swap_total_bytes),
                cpu_percent: before
                    .zip(now)
                    .and_then(|(before, now)| cpu_percent(before, now))
                    .unwrap_or(-1.0),
                boot: boot_time(),
                battery: battery(),
            }
        }),
    });
    Ok((machines, service))
}

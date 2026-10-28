//! The whole service against a fake GitHub and a fake console, with real sockets and the real clients: a job from
//! GitHub's queue to the console's page, what GitHub and the console do when they fail, and a Runner that starts
//! again. Only the vm-hosts and the Mac's readings are stand-ins.

mod support;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_budget::{Battery, FakeHost, GIB, Millis, Mode, Settings, Size};
use cf_console_client::{Client, Enroll, JobEvent, MAX_RESULTS, OUTBOX_CAPACITY, Outbox, enroll};
use cf_daemon::names::job_name;
use cf_daemon::testing::{
    CrashingStore, FakeSettings, FakeUpdate, FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for,
};
use cf_daemon::update::{SettingsChange, SettingsLook};
use cf_daemon::{
    Claim, Console, HostExtras, JobReport, Nudge, OutboxReports, RESULT_WAIT, Reports, Roster, Service,
    ServiceConfig, ServiceParts, Slots,
};
use cf_fake_console::FakeConsole;
use cf_fake_github::harness::{REPO, TestClock, app, fast, until};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, JobState, Options};
use cf_ghapp::{Clock as _, Http, RetryPolicy};
use cf_guest_proto::{Pressure as Stalls, Psi, StopReason};
use cf_scaleset::ScaleSetClient;
use cf_secret::Secret;
use cf_vmhost::End;
use proptest::prelude::*;
use serde_json::{Value, json};
use support::pick;

const MACHINE: &str = "mac-b";
const WORKFLOW: &str = "on: push\njobs:\n  build:\n    runs-on: [self-hosted, cf-2g]\n    steps: []\n";

type TestService = Service<FakeHost, FakeVms>;

struct Rig {
    github: FakeGithub,
    console: FakeConsole,
    parts: TestParts,
    /// The Runner's wall clock, which the scalers read.
    clock: Arc<TestClock>,
    /// The records, at which the Runner can be made to die ([`Rig::crash_at`]).
    store: Arc<CrashingStore>,
    /// Whether the Runner died at a write: nothing it reports after that reaches the outbox.
    dead: Arc<AtomicBool>,
    machines: Arc<TestMachines>,
    service: Arc<TestService>,
    outbox: Arc<Outbox>,
    token: Secret<String>,
    folder: tempfile::TempDir,
    /// The battery the Mac says it has.
    battery: Arc<Mutex<Option<Battery>>>,
    /// How the Runner updates itself.
    updates: Arc<FakeUpdate>,
    /// The Runner's settings file.
    settings: Arc<FakeSettings>,
    /// How long an update waits for the machines that stand, in wall time.
    update_limit: Duration,
    /// Lets time pass on the Mac and runs the service's rounds, as its loop does, and counts them.
    pump: Option<(Arc<AtomicBool>, tokio::task::JoinHandle<()>, Arc<AtomicUsize>)>,
}

/// The rounds of the Runner within which [`Rig::settles`] wants what it waits for: on an idle Mac a round takes some
/// 20 ms and the slowest wait some 30 rounds. A loaded Mac makes each round slower, not more of them needed.
const ROUNDS: usize = 400;
/// How long one round may take before [`Rig::settles`] calls the Runner stuck.
const ROUND_STALL: Duration = Duration::from_secs(30);

/// How long a real Mac's update waits for the machines that stand.
fn default_update_limit() -> Duration {
    ServiceConfig::new(MACHINE, "", "", RUNNING).update_limit
}

/// A test that fails while a build waits ([`FakeUpdate::hold_builds`]) ends: the runtime waits for the build.
impl Drop for Rig {
    fn drop(&mut self) {
        self.updates.release_builds();
    }
}

fn http() -> Http {
    Http::new(Duration::from_secs(5), fast())
}

impl Rig {
    /// A rig whose scale set has its session, with the pump stopped: the tests pass the time.
    async fn with_session() -> Rig {
        let mut rig = Rig::new(2).await;
        rig.service.discover().await;
        until("the scale set's session", || {
            rig.github.scale_sets().iter().any(|s| s.has_session)
        })
        .await;
        rig.stop_pump().await;
        rig
    }

    /// The report to the console a second on.
    fn report(&self) -> Value {
        pass(&self.machines, 1);
        serde_json::to_value(self.service.status()).expect("a report")
    }

    /// A Mac with room for `machines` machines, enrolled at the console, serving [`REPO`] with one workflow. An
    /// update waits as long for the machines that stand as on a real Mac: a loaded Mac does not cut it short.
    async fn new(machines: u32) -> Rig {
        Rig::with(settings_for(machines), default_update_limit()).await
    }

    /// A host with these budget numbers, enrolled at the console, serving [`REPO`] with one workflow.
    async fn with_settings(settings: Settings) -> Rig {
        Rig::with(settings, default_update_limit()).await
    }

    /// [`Rig::new`], whose updates wait `update_limit` for the machines that stand.
    async fn limited(machines: u32, update_limit: Duration) -> Rig {
        Rig::with(settings_for(machines), update_limit).await
    }

    async fn with(settings: Settings, update_limit: Duration) -> Rig {
        let github = FakeGithub::start(Options {
            poll_timeout: Duration::from_millis(150),
            auto_assign: true,
            ..Options::default()
        })
        .await;
        github.add_repo(REPO, true);
        github.set_workflow(REPO, "ci.yml", WORKFLOW);
        let console = FakeConsole::start().await;
        console.add_code("one-time-code");
        let enrolled = enroll(
            &Http::new(Duration::from_secs(5), RetryPolicy { retries: 0, ..fast() }),
            &console.url(),
            &Enroll {
                code: Secret::new("one-time-code".to_string()),
                machine: MACHINE.into(),
                hostname: "Build-Mac-2.local".into(),
                model: "Mac16,12".into(),
                version: "0.1.0".into(),
                cpus: 16,
                memory_bytes: 64 << 30,
                os: cf_console_client::Os::this(),
            },
        )
        .await
        .expect("enrolled");
        let folder = tempfile::tempdir().expect("folder");
        let outbox = Arc::new(Outbox::open(folder.path().join("outbox.json"), OUTBOX_CAPACITY));
        let clock = Arc::new(TestClock::default());
        let store = Arc::new(CrashingStore::default());
        let dead = Arc::new(AtomicBool::new(false));
        let parts = TestParts {
            vms: FakeVms::default(),
            store: store.clone(),
            roster: Arc::new(Roster::new()),
            clock: clock.clone(),
            reports: Arc::new(ReportLog::default()),
            settings,
            nudges: Arc::new(AtomicUsize::new(0)),
            boot: "start-1".into(),
            bell: Arc::new(Nudge::default()),
        };
        parts
            .reports
            .forward_to(Arc::new(Gated(dead.clone(), OutboxReports(outbox.clone()))));
        let machines = parts.machines(None);
        let battery = Arc::new(Mutex::new(None));
        let updates = Arc::new(FakeUpdate::default());
        let settings = Arc::new(FakeSettings::default());
        let mut rig = Rig {
            service: Arc::new(service(
                &github,
                &console,
                &parts,
                &machines,
                &enrolled.token,
                &battery,
                (&outbox, Some(&updates), &settings, folder.path(), update_limit),
            )),
            github,
            console,
            parts,
            clock,
            store,
            dead,
            machines,
            outbox,
            token: enrolled.token,
            folder,
            battery,
            updates,
            settings,
            update_limit,
            pump: None,
        };
        rig.pump();
        rig
    }

    fn pump(&mut self) {
        let stop = Arc::new(AtomicBool::new(false));
        let rounds = Arc::new(AtomicUsize::new(0));
        let (stopped, done, machines, service) = (
            stop.clone(),
            rounds.clone(),
            self.machines.clone(),
            self.service.clone(),
        );
        let task = tokio::spawn(async move {
            while !stopped.load(Ordering::Relaxed) {
                pass(&machines, 1);
                service.sweep().await;
                service.look_at_settings();
                service.report().await;
                done.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        });
        self.pump = Some((stop, task, rounds));
    }

    /// Waits until `condition` holds, for as many of the Runner's rounds as [`ROUNDS`]: what the rounds bring about
    /// comes within so many of them however slow a loaded Mac makes each. Without the pump it waits as [`until`].
    async fn settles(&self, what: &str, condition: impl Fn() -> bool) {
        let Some((_, _, rounds)) = &self.pump else {
            return until(what, condition).await;
        };
        let began = std::time::Instant::now();
        let first = rounds.load(Ordering::Relaxed);
        let mut last = (first, began);
        while !condition() {
            let now = rounds.load(Ordering::Relaxed);
            if now != last.0 {
                last = (now, std::time::Instant::now());
            }
            let passed = now - first;
            assert!(
                passed < ROUNDS && last.1.elapsed() < ROUND_STALL,
                "gave up waiting for {what}: {passed} rounds of the Runner in {:.1?} went by without it, the last \
                 {:.1?} ago; the console last heard the reason {}",
                began.elapsed(),
                last.1.elapsed(),
                self.console
                    .reports()
                    .last()
                    .map_or(Value::Null, |report| report["status"]["reason"].clone()),
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn stop_pump(&mut self) {
        if let Some((stop, task, _)) = self.pump.take() {
            stop.store(true, Ordering::Relaxed);
            task.await.expect("pump");
        }
    }

    /// The Runner's process dies and a new one starts: nothing is closed at GitHub, the vm-hosts go on. One that died
    /// at a write already ([`Rig::crash_at`]) is only started again.
    async fn restart(&mut self) {
        self.restart_after(|_| {}).await;
    }

    /// Starts again as [`Rig::restart`] does, once `edit` changed what the dead Runner left on disk.
    async fn restart_after(&mut self, edit: impl FnOnce(&Rig)) {
        self.start_again(edit).await;
        self.pump();
    }

    /// Starts again as [`Rig::restart`] does, as the program an update installed: it takes `pending` up before its
    /// first report, as the binary does, so the command the console sends again is not done again.
    async fn restart_into(&mut self, pending: &cf_daemon::update::Pending) {
        self.start_again(|_| {}).await;
        self.service.take_up(pending);
        self.pump();
    }

    /// The new Runner of [`Rig::restart_after`], not yet doing its rounds.
    async fn start_again(&mut self, edit: impl FnOnce(&Rig)) {
        // From this moment nothing the dying Runner asks reaches GitHub: an answer it got is the last it gets.
        self.github.set_down(true);
        self.stop_pump().await;
        self.service.kill();
        if self.store.crashed() {
            // The listeners end at their next wait; until then GitHub and the vm-hosts keep refusing them.
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.parts.vms.freeze(false);
        }
        self.store.revive();
        edit(self);
        self.dead.store(false, Ordering::Relaxed);
        self.github.set_down(false);
        let host = self.machines.with_host(|host| host.clone());
        self.parts.roster = Arc::new(Roster::new());
        self.parts.bell = Arc::new(Nudge::default());
        self.outbox = Arc::new(Outbox::open(
            self.folder.path().join("outbox.json"),
            OUTBOX_CAPACITY,
        ));
        self.parts.reports.forward_to(Arc::new(Gated(
            self.dead.clone(),
            OutboxReports(self.outbox.clone()),
        )));
        self.machines = self.parts.restored(host);
        self.service = Arc::new(service(
            &self.github,
            &self.console,
            &self.parts,
            &self.machines,
            &self.token,
            &self.battery,
            (
                &self.outbox,
                Some(&self.updates),
                &self.settings,
                self.folder.path(),
                self.update_limit,
            ),
        ));
        pass(&self.machines, 61);
    }

    /// The Runner dies at its `writes`th write from now, before or after it is `kept`: from that moment nothing it does
    /// reaches GitHub, the vm-hosts or the console's outbox, as for a process that is gone.
    fn crash_at(&self, writes: usize, kept: bool) {
        let (dead, github, vms) = (self.dead.clone(), self.github.clone(), self.parts.vms.clone());
        self.store.crash_at(writes, kept, move || {
            dead.store(true, Ordering::Relaxed);
            github.set_down(true);
            vms.freeze(true);
        });
    }

    /// Time passes on the Runner's clock and on GitHub's alike.
    fn later(&self, time: Duration) {
        self.clock.advance(time);
        let ahead = self
            .clock
            .now()
            .duration_since(std::time::SystemTime::now())
            .unwrap_or_default();
        self.github
            .set_clock_offset(i64::try_from(ahead.as_secs()).unwrap_or(i64::MAX));
    }

    /// Queues a job once the Mac's last two polls offered room, so GitHub hands it over at once.
    async fn queue(&self) -> i64 {
        self.settles("the Mac to offer room", || self.github.offers_room(REPO))
            .await;
        self.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build")
    }

    /// The Actions runners whose machine stands.
    /// The Actions runners whose machine stands and has not said it is up.
    fn booting(&self) -> Vec<String> {
        self.parts
            .vms
            .booting()
            .into_iter()
            .map(|job| format!("{MACHINE}-{job}"))
            .collect()
    }

    fn standing(&self) -> Vec<String> {
        self.parts
            .vms
            .standing()
            .into_iter()
            .map(|job| format!("{MACHINE}-{job}"))
            .collect()
    }

    /// A queued job gets a machine and GitHub hands it over. Says which Actions runner has it.
    async fn job_running(&self) -> String {
        let before = self.standing();
        self.queue().await;
        self.settles("a machine for the job", || self.standing().len() > before.len())
            .await;
        let runner = self
            .standing()
            .into_iter()
            .find(|r| !before.contains(r))
            .expect("runner");
        self.github.start_job(&runner);
        runner
    }

    /// The job on this Actions runner ends at GitHub and its machine goes.
    fn finish(&self, runner: &str, result: &str) {
        self.parts.vms.end(job_name(runner), End::Ok);
        self.github.complete_job(runner, result);
    }

    /// The job events the console took, as `(id, end)`.
    fn events(&self) -> Vec<(String, Value)> {
        self.console
            .job_events()
            .into_iter()
            .map(|event| {
                (
                    event["id"].as_str().unwrap_or("").to_string(),
                    event["end"].clone(),
                )
            })
            .collect()
    }

    /// Nothing is left of any job on the Mac or at GitHub.
    async fn nothing_left(&self) {
        until("no registration at GitHub", || self.github.runners().is_empty()).await;
        until("no machine", || self.parts.vms.standing().is_empty()).await;
        until("no slot", || self.machines.snapshot().budget.slots.is_empty()).await;
        until("no record", || self.parts.store.load().is_empty()).await;
        until("no report waits", || self.outbox.is_empty()).await;
        assert_eq!(self.parts.vms.files(), Vec::<String>::new());
    }
}

/// Reports that reach the outbox while the Runner lives.
struct Gated(Arc<AtomicBool>, OutboxReports);

impl Reports for Gated {
    fn job(&self, report: JobReport) {
        if !self.0.load(Ordering::Relaxed) {
            self.1.job(report);
        }
    }
}

fn service(
    github: &FakeGithub,
    console: &FakeConsole,
    parts: &TestParts,
    machines: &Arc<TestMachines>,
    token: &Secret<String>,
    battery: &Arc<Mutex<Option<Battery>>>,
    kept: (
        &Arc<Outbox>,
        Option<&Arc<FakeUpdate>>,
        &Arc<FakeSettings>,
        &std::path::Path,
        Duration,
    ),
) -> TestService {
    service_with(github, console, parts, machines, token, battery, kept, |_| {})
}

/// [`service`] with its configuration changed by `tweak`.
#[allow(clippy::too_many_arguments)]
fn service_with(
    github: &FakeGithub,
    console: &FakeConsole,
    parts: &TestParts,
    machines: &Arc<TestMachines>,
    token: &Secret<String>,
    battery: &Arc<Mutex<Option<Battery>>>,
    // What stays across a daemon's starts: the outbox, the update it builds (none for a Runner with nowhere to
    // update from), its settings file, the folder of its files, how long an update waits for the machines that
    // stand.
    (outbox, updates, settings, folder, update_limit): (
        &Arc<Outbox>,
        Option<&Arc<FakeUpdate>>,
        &Arc<FakeSettings>,
        &std::path::Path,
        Duration,
    ),
    tweak: impl FnOnce(&mut ServiceConfig),
) -> TestService {
    let battery = battery.clone();
    let mut config = ServiceConfig::new(
        MACHINE,
        "Build-Mac-2.local",
        "Apple M5 (Mac17,3)",
        "0.1.0+446125c0a1b2",
    );
    config.poll_timeout = Duration::from_secs(5);
    config.policy.host_labels = vec!["cf-macos".into()];
    config.policy.public_repos = vec![OPEN.into()];
    config.sweep_again = Duration::from_millis(50);
    config.update_limit = update_limit;
    config.backoff = (
        Duration::from_millis(20),
        Duration::from_millis(100),
        Duration::from_secs(60),
    );
    tweak(&mut config);
    Service::new(ServiceParts {
        app: app(github, http(), parts.clock.clone()),
        machines: machines.clone(),
        roster: parts.roster.clone(),
        clock: parts.clock.clone(),
        nudge: parts.bell.clone(),
        console: Some(Console {
            client: Client::new(&console.url(), Secret::new(token.expose().clone()), http()),
            outbox: outbox.clone(),
        }),
        config,
        extras: Box::new(move || HostExtras {
            load: [1.5, 1.0, 0.5],
            disk_total_bytes: 2000 << 30,
            swap_total_bytes: 0,
            cpu_percent: 12.0,
            boot: Some(1_790_000_000),
            battery: *battery.lock().expect("battery"),
        }),
        updates: updates.map(|updates| updates.clone() as Arc<dyn cf_daemon::update::SelfUpdate>),
        results: Some(folder.join("results.json")),
        settings: Some(settings.clone()),
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_goes_from_githubs_queue_to_the_consoles_page_and_leaves_nothing() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the scale set", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    let sets = rig.github.scale_sets();
    assert_eq!(sets.len(), 1);
    assert!(sets[0].name.starts_with("mac-b--"), "{}", sets[0].name);
    assert_eq!(sets[0].labels, ["cf-2g", "self-hosted"]);

    let runner = rig.job_running().await;
    until("the start at the console", || rig.events().len() == 1).await;
    let started = &rig.console.job_events()[0];
    assert_eq!(started["id"], json!(format!("{runner}/started")));
    assert_eq!(started["kind"], json!("started"));
    assert_eq!(started["runner"], json!(runner));
    assert_eq!(started["repo"], json!(REPO));
    assert_eq!(started["job"], json!("build"));
    assert_eq!(started["end"], Value::Null);
    assert_eq!(started["machine"]["size"], json!("cf-2g"));
    assert_eq!(started["machine"]["memoryBytes"], json!(2u64 << 30));
    assert_eq!(started["machine"]["origin"], json!("assigned"));
    // The console's page of the Mac shows the machine and its job.
    until("the machine in a report", || {
        rig.console.reports().last().is_some_and(|report| {
            report["status"]["runners"][0]["name"] == json!(runner)
                && report["status"]["runners"][0]["job"]["name"] == json!("build")
        })
    })
    .await;
    let report = rig.console.reports().last().cloned().expect("report");
    assert_eq!(report["status"]["machine"], json!(MACHINE));
    assert_eq!(report["status"]["open"], json!(true));
    assert_eq!(
        report["status"]["maxJobs"],
        json!(1),
        "4 GB of machines: one of the commonest size"
    );
    assert_eq!(report["status"]["scaleSets"][0]["repo"], json!(REPO));
    assert_eq!(report["status"]["scaleSets"][0]["id"], json!(sets[0].id));
    assert_eq!(report["status"]["host"]["memoryBytes"], json!(64u64 << 30));
    assert_eq!(report["status"]["host"]["memoryPressure"], json!(1));
    assert_eq!(report["status"]["host"]["load1"], json!(1.5));
    assert_eq!(
        report["status"]["host"]["bootTime"],
        json!("2026-09-21T14:13:20Z")
    );
    assert_eq!(report["activity"]["cpuPercent"], json!(12.0));
    // What the job uses and stalls on reaches the page, page cache and the host's wait for its CPUs included.
    let job = job_name(&runner).to_string();
    rig.parts.vms.usage(&job, 700 << 20, 3 << 20);
    let psi = |some, full| Psi { some, full };
    let strained = Stalls {
        memory: psi(4210, 1300),
        cpu: psi(800, 5),
        io: psi(60, 20),
    };
    rig.parts.vms.strain(&job, 40, strained, psi(2500, 300));
    until("the job's stalls in a report", || {
        rig.console
            .reports()
            .last()
            .is_some_and(|report| report["status"]["runners"][0]["usage"]["stalls"].is_object())
    })
    .await;
    let report = rig.console.reports().last().cloned().expect("report");
    assert_eq!(
        report["status"]["runners"][0]["usage"],
        json!({
            "cpuPercent": 40.0,
            "memoryBytes": 700u64 << 20,
            "fileBytes": 0,
            "stalls": {
                "memory": {"some": 4210, "full": 1300},
                "cpu": {"some": 800, "full": 5},
                "io": {"some": 60, "full": 20},
            },
            "hostCpu": {"some": 2500, "full": 300},
        })
    );
    // Beside the machine that runs, its 4 GB of machines take one more cf-2g and no cf-4g; larger sizes, never.
    assert_eq!(
        report["status"]["sizes"],
        json!([
            {"size": "cf-2g", "free": 1},
            {"size": "cf-4g", "free": 0, "why": "full"},
            {"size": "cf-6g", "free": -1, "why": "never"},
            {"size": "cf-8g", "free": -1, "why": "never"},
        ])
    );

    rig.finish(&runner, "succeeded");
    until("the end at the console", || rig.events().len() == 2).await;
    let completed = &rig.console.job_events()[1];
    assert_eq!(completed["id"], json!(format!("{runner}/completed")));
    assert_eq!(completed["kind"], json!("completed"));
    assert_eq!(completed["result"], json!("succeeded"));
    assert_eq!(completed["end"], json!("success"));
    assert_eq!(
        completed["outOfMemory"],
        Value::Null,
        "left out when the job did not run out of memory"
    );
    assert_eq!(completed["machine"]["wallMs"], json!(4200));
    assert_eq!(completed["machine"]["bootMs"], json!(1200));
    assert_eq!(
        completed["machine"]["stallsPeak"],
        json!({
            "memory": {"some": 4210, "full": 1300},
            "cpu": {"some": 800, "full": 5},
            "io": {"some": 60, "full": 20},
        })
    );
    assert_eq!(
        completed["machine"]["hostCpuPeak"],
        json!({"some": 2500, "full": 300})
    );
    rig.nothing_left().await;
    assert_eq!(rig.events().len(), 2, "each once");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_size_the_mac_never_runs_gets_no_scale_set_and_a_label_from_a_variable_is_read() {
    let rig = Rig::new(3).await;
    // Room for 6 GB in all: a machine of 8 GB never fits; one of 4 GB does, and its label comes from a variable.
    rig.github.set_workflow(
        REPO,
        "big.yml",
        "on: push\njobs:\n  big:\n    runs-on: [self-hosted, cf-8g]\n    steps: []\n  sized:\n    runs-on: ${{ vars.CI_RUNNER }}\n    steps: []\n",
    );
    rig.github.set_variable(REPO, "CI_RUNNER", "cf-4g");
    rig.service.discover().await;
    until("two scale sets", || rig.github.scale_sets().len() == 2).await;
    let mut labels: Vec<Vec<String>> = rig.github.scale_sets().into_iter().map(|s| s.labels).collect();
    labels.sort();
    assert_eq!(labels, [vec!["cf-2g", "self-hosted"], vec!["cf-4g"]]);
    assert_eq!(rig.service.discovery_error(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variables_github_could_not_show_for_a_moment_change_no_scale_set() {
    let rig = Rig::new(3).await;
    rig.github.set_workflow(
        REPO,
        "sized.yml",
        "on: push\njobs:\n  sized:\n    runs-on: ${{ vars.CI_RUNNER }}\n    steps: []\n",
    );
    rig.github.set_variable(REPO, "CI_RUNNER", "cf-4g");
    rig.service.discover().await;
    until("two scale sets", || rig.github.scale_sets().len() == 2).await;
    let before: BTreeSet<(i64, Vec<String>)> = rig
        .github
        .scale_sets()
        .into_iter()
        .map(|s| (s.id, s.labels))
        .collect();
    assert!(
        before.iter().any(|(_, labels)| labels == &["cf-4g"]),
        "{before:?}"
    );

    // GitHub is unwell for a moment: that is not a variable taken away.
    rig.github.fail(
        "actions/variables",
        Reply::status_json(503, &json!({"message": "Service unavailable"})),
        100,
    );
    rig.service.discover().await;
    let error = rig.service.discovery_error().expect("said");
    assert!(
        error.contains("org/app: its variables could not be read"),
        "{error}"
    );
    let after: BTreeSet<(i64, Vec<String>)> = rig
        .github
        .scale_sets()
        .into_iter()
        .map(|s| (s.id, s.labels))
        .collect();
    assert_eq!(
        after, before,
        "a scale set was deleted or made for a variable nobody could read"
    );
    assert_eq!(rig.service.sets().len(), 2, "a listener was closed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn variables_github_will_not_show_are_named_and_the_labels_written_out_are_still_served() {
    let rig = Rig::new(2).await;
    // The App was not given "Variables: read".
    rig.github.fail(
        "actions/variables",
        Reply::status_json(403, &json!({"message": "Resource not accessible by integration"})),
        100,
    );
    rig.service.discover().await;
    until("the scale set", || rig.github.scale_sets().len() == 1).await;
    let error = rig.service.discovery_error().expect("said");
    assert!(
        error.contains("org/app: its variables could not be read"),
        "{error}"
    );
    until("the console hears of it", || {
        rig.console.reports().last().is_some_and(|r| {
            r["status"]["discoveryError"]
                .as_str()
                .is_some_and(|e| e.contains("variables"))
        })
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repository_that_cannot_be_read_keeps_its_scale_sets_and_one_nothing_asks_for_is_deleted() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the scale set", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    let runner = rig.job_running().await;

    // GitHub fails while the workflows are read: nothing is concluded from that.
    rig.github
        .fail("contents/.github/workflows", Reply::text(502, "bad gateway"), 100);
    rig.github.fail("git/trees", Reply::text(502, "bad gateway"), 100);
    rig.service.discover().await;
    assert!(rig.service.discovery_error().is_some());
    assert_eq!(rig.service.sets().len(), 1, "the scale set is still served");
    assert!(rig.github.scale_sets()[0].has_session);
    assert_eq!(rig.standing(), [runner.as_str()], "and its job goes on");
    rig.finish(&runner, "succeeded");
    rig.nothing_left().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scale_set_no_workflow_asks_for_any_more_is_closed_and_deleted() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the scale set", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    rig.github.set_workflow(
        REPO,
        "ci.yml",
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps: []\n",
    );
    rig.service.discover().await;
    assert_eq!(rig.service.sets(), []);
    assert_eq!(rig.github.scale_sets().len(), 0, "deleted at GitHub");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_repositories_that_ask_for_the_same_labels_each_get_their_jobs() {
    const OTHER: &str = "org/other";
    let rig = Rig::new(3).await;
    rig.github.add_repo(OTHER, true);
    rig.github.set_workflow(OTHER, "ci.yml", WORKFLOW);
    rig.service.discover().await;
    until("a session for each repository's scale set", || {
        let sets = rig.github.scale_sets();
        sets.len() == 2 && sets.iter().all(|s| s.has_session)
    })
    .await;
    let sets = rig.github.scale_sets();
    let repos: BTreeSet<&str> = sets.iter().map(|s| s.repo.as_str()).collect();
    assert_eq!(repos, BTreeSet::from([OTHER, REPO]));
    assert_ne!(sets[0].name, sets[1].name, "a name of its own for each");

    // A second look takes neither for a set nothing asks for.
    rig.service.discover().await;
    assert_eq!(rig.service.sets().len(), 2);
    assert!(rig.github.scale_sets().iter().all(|s| s.has_session));

    rig.queue().await;
    // GitHub hands a queued job only to a scale set whose polls showed room: the other repository's set too.
    until("the other repository's set to offer room", || {
        rig.github.offers_room(OTHER)
    })
    .await;
    rig.github.queue_job(OTHER, &["self-hosted", "cf-2g"], "build");
    until("a machine for the job of each", || rig.standing().len() == 2).await;
    for runner in rig.standing() {
        rig.github.start_job(&runner);
    }
    until("both starts at the console", || rig.events().len() == 2).await;
    let of: BTreeSet<String> = rig
        .console
        .job_events()
        .iter()
        .map(|event| event["repo"].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(of, BTreeSet::from([OTHER.to_string(), REPO.to_string()]));

    // One of them stops asking: only its scale set goes, once its job is over.
    for runner in rig.standing() {
        rig.finish(&runner, "succeeded");
    }
    rig.nothing_left().await;
    rig.github.set_workflow(
        OTHER,
        "ci.yml",
        "on: push\njobs:\n  build:\n    runs-on: ubuntu-latest\n    steps: []\n",
    );
    rig.service.discover().await;
    let left = rig.github.scale_sets();
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0].repo, REPO);
    assert!(left[0].has_session, "the other repository's listener goes on");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whatever_github_answers_the_job_still_runs_and_nothing_is_left() {
    type Fault = Box<dyn Fn(&FakeGithub)>;
    let answers = |matcher: &'static str, reply: fn() -> Reply, times: usize| -> Fault {
        Box::new(move |github| github.fail(matcher, reply(), times))
    };
    // Each row: what goes wrong at GitHub.
    let faults: Vec<(&str, Fault)> = vec![
        (
            "401 as the session opens",
            answers(
                "POST /svc/_apis/runtime/runnerscalesets/10/sessions",
                || Reply::text(401, "bad credentials"),
                2,
            ),
        ),
        (
            "409: a session of before is still open",
            answers(
                "POST /svc/_apis/runtime/runnerscalesets/10/sessions",
                || Reply::status_json(409, &json!({"message": "exists"})),
                3,
            ),
        ),
        (
            "429 on the long poll",
            answers(
                "GET /svc/_apis/runtime/runnerscalesets/10/messages",
                || Reply::text(429, "slow down").with_header("retry-after", "0"),
                3,
            ),
        ),
        (
            "5xx on the long poll",
            answers(
                "GET /svc/_apis/runtime/runnerscalesets/10/messages",
                || Reply::text(503, "unavailable"),
                4,
            ),
        ),
        (
            "5xx as the Actions runner is registered",
            answers("generatejitconfig", || Reply::text(500, "boom"), 2),
        ),
        (
            "5xx as the scale set is made",
            answers(
                "POST /svc/_apis/runtime/runnerscalesets",
                || Reply::text(502, "bad gateway"),
                2,
            ),
        ),
        (
            // GitHub registered the Actions runner and the answer never came: it is found by its name.
            "the answer to a registration is lost",
            Box::new(|github| github.lose_answer("generatejitconfig", 1)),
        ),
        (
            // And GitHub fails when it is looked for: the name stays written down until GitHub answers.
            "the answer to a registration is lost and so is the look for it",
            Box::new(|github| {
                github.lose_answer("generatejitconfig", 1);
                github.fail(
                    "GET /svc/_apis/distributedtask/pools/0/agents",
                    Reply::text(500, "boom"),
                    3,
                );
            }),
        ),
        (
            "the answer to the acquired job's message is lost",
            Box::new(|github| github.lose_answer("GET /svc/_apis/runtime/runnerscalesets/10/messages", 1)),
        ),
    ];
    for (what, fault) in faults {
        let rig = Rig::new(2).await;
        fault(&rig.github);
        rig.service.discover().await;
        let runner = rig.job_running().await;
        rig.finish(&runner, "succeeded");
        until(what, || rig.events().len() == 2).await;
        assert_eq!(rig.events()[1].1, json!("success"), "{what}");
        rig.nothing_left().await;
        assert_eq!(rig.parts.vms.started().len(), 1, "{what}: one job, one machine");
        assert_eq!(
            rig.github.unspent_faults(),
            Vec::<String>::new(),
            "{what}: GitHub went wrong as often as set"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_poll_that_is_dropped_and_a_session_github_forgets_lose_no_job() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the session", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    let set = rig.github.scale_sets()[0].id;
    // The answer that carries the job never arrives; then GitHub forgets the session altogether.
    rig.github.lose_answer(
        &format!("GET /svc/_apis/runtime/runnerscalesets/{set}/messages"),
        2,
    );
    let first = rig.job_running().await;
    rig.github.lose_session(set);
    let second = rig.job_running().await;
    rig.finish(&first, "succeeded");
    rig.finish(&second, "failed");
    until("both ends", || rig.events().len() == 4).await;
    rig.nothing_left().await;
    assert_eq!(rig.parts.vms.started().len(), 2, "two jobs, two machines");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_github_will_not_remove_yet_is_tried_again_until_it_is_gone() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    // The machine dies; GitHub still believes the job runs and will not remove its Actions runner under it.
    rig.parts.vms.die(job_name(&runner));
    until("the slot is free", || {
        rig.machines.snapshot().budget.slots.is_empty()
    })
    .await;
    until("GitHub refused twice", || {
        rig.github
            .count("DELETE /svc/_apis/distributedtask/pools/0/agents")
            >= 2
    })
    .await;
    assert_eq!(
        rig.github.runners(),
        [runner.as_str()],
        "still registered: GitHub has the say"
    );
    assert_eq!(
        rig.standing(),
        Vec::<String>::new(),
        "no machine is started in its place meanwhile"
    );
    rig.github.complete_job(&runner, "failed");
    until("the end at the console", || rig.events().len() == 2).await;
    assert_eq!(rig.events()[1].1, json!("guest-lost"));
    rig.nothing_left().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn while_the_console_is_away_reports_wait_and_arrive_later_in_order_each_once() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.console.down(true);
    let mut expected = Vec::new();
    for result in ["succeeded", "failed", "canceled"] {
        let runner = rig.job_running().await;
        until("the start waits", || rig.outbox.len() == expected.len() + 1).await;
        rig.finish(&runner, result);
        until("the end waits", || rig.outbox.len() == expected.len() + 2).await;
        expected.push(format!("{runner}/started"));
        expected.push(format!("{runner}/completed"));
    }
    assert_eq!(rig.events(), [], "the console heard nothing");
    until("GitHub is clean without the console", || {
        rig.github.runners().is_empty()
    })
    .await;

    // An answer that is lost on the way back: the console took the events and the Mac cannot know.
    rig.console.down(false);
    rig.console.accept_then_drop_next(1);
    until("everything arrived", || rig.outbox.is_empty()).await;
    let sent = rig.console.job_event_ids();
    assert!(
        sent.len() > 6,
        "the batch whose answer was lost was sent again: {sent:?}"
    );
    let mut ids: Vec<String> = Vec::new();
    for id in sent {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    assert_eq!(ids, expected, "in order; what was sent twice has the same id");
    let ends: Vec<Value> = rig
        .events()
        .into_iter()
        .filter(|e| !e.1.is_null())
        .map(|e| e.1)
        .collect();
    assert!(
        ends.contains(&json!("success"))
            && ends.contains(&json!("failure"))
            && ends.contains(&json!("cancelled"))
    );
    rig.nothing_left().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_consoles_pause_takes_no_new_job_and_its_commands_are_done_or_answered() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.console.pause(true);
    until("the Mac says it is paused", || {
        rig.console
            .reports()
            .last()
            .is_some_and(|r| r["status"]["paused"] == json!(true) && r["status"]["reason"] == json!("paused"))
    })
    .await;
    // GitHub may still assign the job, to a poll that offered a second machine before the pause: it waits.
    rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        rig.standing(),
        [runner.as_str()],
        "a paused Mac starts nothing new"
    );

    let stop = rig
        .console
        .queue_command("stop-runner", Some(json!({"name": runner})));
    let update = rig.console.queue_command("rebuild-image", None);
    until("the machine is stopped", || rig.standing().is_empty()).await;
    until("both commands are answered", || {
        let results: Vec<Value> = rig
            .console
            .reports()
            .iter()
            .flat_map(|r| r["results"].as_array().cloned().unwrap_or_default())
            .collect();
        let of = |id: &str| results.iter().find(|r| r["id"] == json!(id)).cloned();
        of(&stop).is_some_and(|r| r["ok"] == json!(true))
            && of(&update).is_some_and(|r| {
                r["ok"] == json!(false)
                    && r["message"] == json!("this Runner does not do `rebuild-image` yet")
            })
    })
    .await;
    rig.github.complete_job(&runner, "canceled");
    until("the end", || rig.events().len() == 2).await;
    assert_eq!(rig.events()[1].1, json!("stopped"));

    rig.console.pause(false);
    until("the job that waited gets its machine", || {
        rig.standing().len() == 1
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_that_starts_again_adopts_the_job_reports_it_once_and_removes_what_is_left_at_github() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let runs = rig.job_running().await;
    let dies = rig.job_running().await;
    until("both starts at the console", || rig.events().len() == 2).await;

    // The process dies with its sessions open; one vm-host dies while nobody watches.
    rig.restart().await;
    rig.parts.vms.die(job_name(&dies));
    for set in rig.github.scale_sets() {
        // GitHub lets a session that nobody polls expire.
        rig.github.lose_session(set.id);
    }
    rig.service.discover().await;
    until("the session of the new Runner", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    assert!(
        rig.standing().contains(&runs),
        "the machine that runs was left alone"
    );
    assert!(!rig.standing().contains(&dies));
    // GitHub still counts the lost job as assigned: no machine is started in its place, there is no job for one.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.parts.vms.started().len(), 2);
    assert_eq!(
        rig.github.runners().len(),
        2,
        "its registration stays while GitHub holds the job"
    );

    rig.github.complete_job(&dies, "failed");
    rig.finish(&runs, "succeeded");
    until("both ends at the console", || rig.events().len() == 4).await;
    let mut events = rig.events();
    events.sort_by(|a, b| a.0.cmp(&b.0));
    let mut expected = vec![
        (format!("{runs}/started"), Value::Null),
        (format!("{runs}/completed"), json!("success")),
        (format!("{dies}/started"), Value::Null),
        (format!("{dies}/completed"), json!("guest-lost")),
    ];
    expected.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(events, expected);
    rig.nothing_left().await;

    // And the new Runner takes new jobs.
    let next = rig.job_running().await;
    rig.finish(&next, "succeeded");
    until("the new job's end", || rig.events().len() == 6).await;
    rig.nothing_left().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_that_is_told_to_stop_closes_its_sessions_and_leaves_its_machines_running() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.stop_pump().await;
    rig.service.shutdown().await;
    assert!(
        rig.github.scale_sets().iter().all(|s| !s.has_session),
        "sessions are closed"
    );
    assert_eq!(
        rig.standing(),
        [runner.as_str()],
        "the job goes on without the Runner"
    );
    assert_eq!(rig.github.runners(), [runner], "and stays registered");
    let last = rig.console.reports().last().cloned().expect("a last report");
    assert_eq!(last["status"]["open"], json!(false));
    assert_eq!(last["status"]["reason"], json!("draining"));
}

/// A job event as the machines queue one.
fn started(runner: &str) -> JobEvent {
    serde_json::from_value(json!({
        "id": format!("{runner}/started"),
        "kind": "started",
        "runner": runner,
        "at": "2026-10-10T10:00:00Z",
    }))
    .expect("a job event")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_event_is_on_the_consoles_page_within_a_moment_and_a_console_that_is_away_is_not_asked_for_each()
 {
    let mut rig = Rig::new(2).await;
    rig.stop_pump().await;
    let before = rig.console.reports().len();
    let (stop, stopped) = cf_scaleset::shutdown();
    let service = rig.service.clone();
    let running = tokio::spawn(async move { service.run(stopped).await });
    until("the loop's first report", || rig.console.reports().len() > before).await;

    // The console wants to hear again in a minute. The job's start does not wait for it.
    assert!(rig.outbox.push(started("mac-a-j1")));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        rig.console.job_event_ids(),
        ["mac-a-j1/started"],
        "the start is on the console's page within two looks"
    );
    assert!(rig.outbox.is_empty());
    // Told once: with nothing new the console hears again in its turn.
    let told = rig.console.reports().len();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        rig.console.reports().len(),
        told,
        "the console is told again with nothing new"
    );

    // A console that is away is asked once, and then in its turn: not once more for every job event.
    rig.console.down(true);
    let calls = rig.console.calls();
    assert!(rig.outbox.push(started("mac-a-j2")));
    until("the console was asked", || rig.console.calls() > calls).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let asked = rig.console.calls();
    assert!(rig.outbox.push(started("mac-a-j3")));
    assert!(rig.outbox.push(started("mac-a-j4")));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        rig.console.calls(),
        asked,
        "a console that is away was asked for every event"
    );
    assert_eq!(rig.outbox.len(), 3, "and the events wait");

    // Its next report in its turn carries them all, in order.
    rig.console.down(false);
    rig.service.report().await;
    assert_eq!(
        rig.console.job_event_ids(),
        [
            "mac-a-j1/started",
            "mac-a-j2/started",
            "mac-a-j3/started",
            "mac-a-j4/started"
        ]
    );
    // And a console that answers again hears the next event at once.
    assert!(rig.outbox.push(started("mac-a-j5")));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(
        rig.console.job_event_ids().len(),
        5,
        "the next start is on the console's page at once"
    );
    stop.trigger();
    running.await.expect("the loop ends");
}

/// Every command result the console took, in order.
fn results(console: &FakeConsole) -> Vec<Value> {
    console
        .reports()
        .iter()
        .flat_map(|report| report["results"].as_array().cloned().unwrap_or_default())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_was_done_for_the_console_is_said_again_and_not_done_again() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.stop_pump().await;
    assert_eq!(
        rig.service.report().await,
        Duration::from_secs(60),
        "the console says when to report next"
    );
    let stop = rig
        .console
        .queue_command("stop-runner", Some(json!({"name": runner})));
    let other = rig.console.queue_command("rebuild-image", None);
    assert_eq!(
        rig.service.report().await,
        Duration::from_secs(5),
        "sooner while a command waits"
    );
    // The commands came with that answer and were done; the console is away when it is told.
    assert_eq!(rig.standing(), Vec::<String>::new(), "the machine was stopped");
    rig.console.down(true);
    assert_eq!(
        rig.service.report().await,
        Duration::from_secs(60),
        "the Mac's own pace while it is away"
    );
    rig.service.report().await;
    assert_eq!(results(&rig.console), Vec::<Value>::new());
    rig.console.down(false);
    rig.service.report().await;
    rig.service.report().await;
    let heard = results(&rig.console);
    assert_eq!(heard.len(), 2, "each once: {heard:?}");
    // What was done the first time is what is said: a second try would find no machine to stop.
    assert_eq!(heard[0]["id"], json!(stop));
    assert_eq!(heard[0]["ok"], json!(true));
    assert_eq!(heard[0]["message"], json!(format!("{runner} is being stopped")));
    assert_eq!(heard[1]["id"], json!(other));
    assert_eq!(heard[1]["ok"], json!(false));
    assert_eq!(
        heard[1]["message"],
        json!("this Runner does not do `rebuild-image` yet")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_whose_answer_the_console_would_refuse_holds_up_no_report() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.stop_pump().await;
    rig.service.report().await;
    let before = rig.console.reports().len();
    // A result names its command by an id of 1 to 64 characters and says at most 4000 about it.
    rig.console.queue_command_as(&"i".repeat(65), "stop-runner", None);
    rig.console.queue_command_as("", "update", None);
    let wordy = rig.console.queue_command(&"k".repeat(5000), None);
    for _ in 0..3 {
        rig.service.report().await;
    }
    assert_eq!(
        rig.console.reports().len(),
        before + 3,
        "every report after the commands came still arrives"
    );
    let heard = results(&rig.console);
    assert_eq!(
        heard.len(),
        1,
        "only the one that can be answered, once: {heard:?}"
    );
    assert_eq!(heard[0]["id"], json!(wordy));
    assert_eq!(heard[0]["ok"], json!(false));
    let said = heard[0]["message"].as_str().expect("message");
    assert!(said.starts_with("this Runner does not do `kkkk"), "{said}");
    assert_eq!(said.chars().count(), 1000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mac_whose_token_the_console_does_not_know_asks_rarely_and_keeps_its_reports() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    until("the start at the console", || rig.events().len() == 1).await;
    rig.stop_pump().await;
    let stranger = service(
        &rig.github,
        &rig.console,
        &rig.parts,
        &rig.machines,
        &Secret::new("not-this-macs-token".to_string()),
        &rig.battery,
        (
            &rig.outbox,
            Some(&rig.updates),
            &rig.settings,
            rig.folder.path(),
            rig.update_limit,
        ),
    );
    rig.finish(&runner, "succeeded");
    until("the end waits to be sent", || {
        pass(&rig.machines, 1);
        rig.outbox.len() == 1
    })
    .await;
    let reports = rig.console.reports().len();
    assert_eq!(
        stranger.report().await,
        Duration::from_secs(300),
        "five times as rarely"
    );
    assert_eq!(rig.console.reports().len(), reports, "the console took nothing");
    assert_eq!(rig.outbox.len(), 1, "and the job's end still waits");
    rig.service.report().await;
    assert_eq!(rig.events().len(), 2, "it arrives with the Mac's own token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uninstall_stops_every_machine_and_leaves_nothing_at_github_or_the_console() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let busy = rig.job_running().await;
    rig.queue().await;
    until("a second machine", || rig.standing().len() == 2).await;

    // GitHub ends the job of an Actions runner that went away, in its own time.
    let github = rig.github.clone();
    let ends = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        github.complete_job(&busy, "failed");
    });
    let left = rig.service.retire(Duration::from_secs(8), true).await;
    ends.await.expect("ends");
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(rig.standing(), Vec::<String>::new());
    assert_eq!(rig.github.runners(), Vec::<String>::new());
    assert_eq!(rig.github.scale_sets().len(), 0, "its scale set is deleted");
    assert_eq!(rig.parts.store.load().len(), 0);
    assert_eq!(rig.console.left(), [MACHINE]);
    let ends: Vec<Value> = rig
        .events()
        .into_iter()
        .map(|event| event.1)
        .filter(|end| !end.is_null())
        .collect();
    assert_eq!(ends, [json!("stopped")], "the job that was stopped is reported");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uninstall_reports_the_job_it_stopped_without_waiting_for_a_word_github_cannot_send() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let busy = rig.job_running().await;
    until("the start at the console", || rig.events().len() == 1).await;
    rig.stop_pump().await;

    // Half a minute passes on the Mac while the uninstall runs, and then no more: far less than the time a job's
    // end waits for GitHub's word while somebody listens for it.
    let machines = rig.machines.clone();
    let ticks = tokio::spawn(async move {
        for second in 0.. {
            if second < 30 {
                pass(&machines, 1);
            } else {
                machines.tick();
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
    });
    let github = rig.github.clone();
    let runner = busy.clone();
    let lets_go = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        github.complete_job(&runner, "failed");
    });
    let left = rig.service.retire(Duration::from_secs(6), true).await;
    ticks.abort();
    lets_go.await.expect("GitHub lets the job go");
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(
        rig.events(),
        [
            (format!("{busy}/started"), Value::Null),
            (format!("{busy}/completed"), json!("stopped"))
        ],
        "the job that was stopped is at the console before the Mac leaves it"
    );
    assert_eq!(rig.parts.store.load().len(), 0, "and no record of it stays");
    assert_eq!(rig.console.left(), [MACHINE]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uninstall_says_what_it_could_not_remove() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let busy = rig.job_running().await;
    rig.stop_pump().await;
    // Nobody looks at the machines and GitHub never ends the job: the machine stands, its registration stays.
    rig.parts.vms.ignore_stops(true);
    let left = rig.service.retire(Duration::from_millis(300), false).await;
    assert_eq!(left.len(), 2, "{left:?}");
    assert_eq!(left[0], format!("the machine of {busy} still stands"));
    assert!(left[1].ends_with("still has Actions runners"), "{left:?}");
    assert_eq!(
        rig.console.left(),
        Vec::<String>::new(),
        "the Mac was not asked to leave the console"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_of_a_repository_no_look_has_shown_is_removed_all_the_same() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    // The Runner starts again and its machine died meanwhile; GitHub cannot be asked for the repositories.
    rig.restart().await;
    rig.parts.vms.die(job_name(&runner));
    rig.github.complete_job(&runner, "failed");
    // No look at GitHub ran in this Runner: the registration's repository is found by its name.
    until("the end at the console", || rig.events().len() == 2).await;
    rig.nothing_left().await;
    assert_eq!(rig.service.sets(), [], "no scale set was served to do it");
}

/// A job pinned to a kind of host (`[cf-4g, cf-macos]`, from a variable read with fromJSON) is served only by a
/// host whose config lists every host label it asks for; the others leave it to the hosts that have them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_that_asks_for_host_labels_is_served_only_by_a_host_that_has_them() {
    let rig = Rig::new(3).await;
    rig.github.set_workflow(
        REPO,
        "pinned.yml",
        "on: push\njobs:\n  heavy:\n    runs-on: ${{ fromJSON(vars.CI_RUNNER_HEAVY || '\"ubuntu-latest\"') }}\n    steps: []\n  linux:\n    runs-on: [cf-4g, cf-linux]\n    steps: []\n  both:\n    runs-on: [cf-4g, cf-linux, cf-macos]\n    steps: []\n",
    );
    rig.github
        .set_variable(REPO, "CI_RUNNER_HEAVY", r#"["cf-4g","cf-macos"]"#);
    let (served, problems) = rig.service.survey().await;
    assert_eq!(
        served,
        [
            "org/app: [cf-2g, self-hosted] as cf-2g",
            "org/app: [cf-4g, cf-macos] as cf-4g"
        ]
    );
    assert_eq!(problems, Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_survey_says_what_would_be_served_and_what_cannot_be_read_and_changes_nothing_at_github() {
    let rig = Rig::new(3).await;
    rig.github.set_workflow(
        REPO,
        "sized.yml",
        "on: push\njobs:\n  sized:\n    runs-on: ${{ vars.CI_RUNNER }}\n    steps: []\n",
    );
    rig.github.set_variable(REPO, "CI_RUNNER", "cf-4g");
    let (served, problems) = rig.service.survey().await;
    assert_eq!(
        served,
        [
            "org/app: [cf-2g, self-hosted] as cf-2g",
            "org/app: [cf-4g] as cf-4g"
        ]
    );
    assert_eq!(problems, Vec::<String>::new());

    // The App may not read variables: the survey says so, which is what an install is checked with.
    rig.github.fail(
        "actions/variables",
        Reply::status_json(403, &json!({"message": "Resource not accessible by integration"})),
        100,
    );
    let (served, problems) = rig.service.survey().await;
    // A label that comes from a variable is not known then: it stands for the pool's label, at its size.
    assert_eq!(
        served,
        [
            "org/app: [self-hosted] as cf-4g",
            "org/app: [cf-2g, self-hosted] as cf-2g"
        ]
    );
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0].starts_with("org/app: its variables could not be read"),
        "{problems:?}"
    );

    assert_eq!(rig.github.scale_sets().len(), 0, "no scale set was made");
    let writes: Vec<String> = rig
        .github
        .requests()
        .into_iter()
        .map(|request| request.line)
        .filter(|line| !line.starts_with("GET ") && !line.contains("/access_tokens"))
        .collect();
    assert_eq!(writes, Vec::<String>::new(), "GitHub was only read");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_long_list_of_problems_with_the_repositories_does_not_make_the_console_refuse_the_report() {
    let mut rig = Rig::new(2).await;
    rig.stop_pump().await;
    // Forty repositories whose variables the App may not read: the problems together are far over a text the
    // console takes (4000 characters), and it refuses the whole report that carries one.
    for n in 0..40 {
        let repo = format!("org/repository-with-a-long-name-{n:02}");
        rig.github.add_repo(&repo, true);
        rig.github.set_workflow(&repo, "ci.yml", WORKFLOW);
    }
    rig.github.fail(
        "actions/variables",
        Reply::status_json(403, &json!({"message": "Resource not accessible by integration"})),
        1000,
    );
    rig.service.discover().await;
    let said = rig.service.discovery_error().expect("said");
    assert!(
        said.chars().count() > 4000,
        "the problems are long: {}",
        said.len()
    );
    let before = rig.console.reports().len();
    rig.service.report().await;
    let reports = rig.console.reports();
    assert_eq!(reports.len(), before + 1, "the console took the report");
    let told = reports.last().expect("a report")["status"]["discoveryError"]
        .as_str()
        .expect("the problems are said")
        .to_string();
    assert_eq!(told.chars().count(), 1000);
    assert!(told.starts_with("org/") && told.ends_with('\u{2026}'), "{told}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uninstall_hands_the_console_every_job_event_that_waits_and_not_only_the_first_batch() {
    let mut rig = Rig::new(2).await;
    rig.stop_pump().await;
    let waiting = cf_console_client::REPORT_BATCH * 2 + 30;
    for n in 0..waiting {
        assert!(rig.outbox.push(started(&format!("mac-a-j{n}"))));
    }
    let left = rig.service.retire(Duration::from_secs(6), false).await;
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(rig.console.job_events().len(), waiting);
    assert!(rig.outbox.is_empty());
}

#[tokio::test]
async fn the_log_says_which_scale_set_the_runner_keeps_and_when_it_listens_and_stops() {
    let log = support::Log::start();
    let mut rig = Rig::new(1).await;
    rig.service.discover().await;
    // GitHub has the session before the Runner has its answer and writes the line.
    until("a session, and the line about it", || {
        !log.lines("session is").is_empty()
    })
    .await;
    let set = rig.github.scale_sets()[0].clone();
    assert!(set.has_session);
    assert_eq!(
        log.lines("a scale set at GitHub"),
        [format!(
            "a scale set at GitHub set={} id={} repo={REPO} labels=cf-2g,self-hosted size=\"cf-2g\"",
            set.name, set.id
        )]
    );
    let listens = format!(
        "a session is open: the Runner listens for this scale set's jobs set={}",
        set.name
    );
    assert_eq!(log.lines("session is"), std::slice::from_ref(&listens));
    rig.stop_pump().await;
    rig.service.shutdown().await;
    assert_eq!(
        log.lines("session is"),
        [listens, format!("the session is closed set={}", set.name)]
    );
}

#[tokio::test]
async fn a_report_says_the_memory_that_is_available_what_the_runner_keeps_free_and_the_battery_if_there_is_one()
 {
    let mut rig = Rig::new(2).await;
    rig.stop_pump().await;
    let report = Rig::report;
    // 64 GB installed, of which a quarter and a byte are available: three quarters are in use.
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = (16 << 30) + 1));
    let said = report(&rig);
    let host = &said["status"]["host"];
    let free = host["freeMemoryPercent"].as_f64().expect("a share");
    assert!((free - 25.0).abs() < 1e-6, "{free}");
    assert_eq!(host["availableMemoryBytes"], json!((16u64 << 30) + 1));
    assert_eq!(
        host["memoryFloorBytes"],
        json!(rig.parts.settings.away.floor_bytes)
    );
    assert_eq!(host["memoryBytes"], json!(64u64 << 30));
    assert_eq!(said["model"], json!("Apple M5 (Mac17,3)"));
    assert_eq!(said["status"]["version"], json!("0.1.0+446125c0a1b2"));
    // A Mac without a battery says nothing of one.
    assert_eq!(said["activity"].get("power"), None);

    // Nothing available, and more than is installed (a reading the budget refuses is no reading at all).
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = 0));
    let said = report(&rig);
    assert_eq!(said["status"]["host"]["freeMemoryPercent"], json!(0.0));
    assert_eq!(said["status"]["host"]["availableMemoryBytes"], json!(0));

    for (battery, power) in [
        (
            Battery {
                on_battery: true,
                percent: 7,
                charging: false,
            },
            json!({"onBattery": true, "percent": 7, "charging": false}),
        ),
        (
            Battery {
                on_battery: false,
                percent: 100,
                charging: false,
            },
            json!({"onBattery": false, "percent": 100, "charging": false}),
        ),
        (
            Battery {
                on_battery: false,
                percent: 0,
                charging: true,
            },
            json!({"onBattery": false, "percent": 0, "charging": true}),
        ),
    ] {
        *rig.battery.lock().expect("battery") = Some(battery);
        assert_eq!(report(&rig)["activity"]["power"], power);
    }
    // The console takes the report as it is.
    let before = rig.console.reports().len();
    rig.service.report().await;
    let reports = rig.console.reports();
    assert_eq!(reports.len(), before + 1);
    assert_eq!(
        reports[before]["activity"]["power"],
        json!({"onBattery": false, "percent": 0, "charging": true})
    );
}

/// The Air and the mini, 2026-10-12. Each of a Mac's scale sets offers GitHub the Mac's free slot with its long
/// poll, and GitHub assigns against that number for as long as the poll stands. A slot one repository's job took
/// must end the other repositories' polls, or their next job is assigned to this Mac, where it waits in line,
/// instead of staying at GitHub for a Mac with room.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_for_a_mac_whose_last_slot_just_went_stays_at_github_and_a_mac_with_room_gets_it() {
    const OTHER: &str = "org/other";
    let rig = Rig::new(1).await;
    rig.github.add_repo(OTHER, true);
    rig.github.set_workflow(OTHER, "ci.yml", WORKFLOW);
    rig.service.discover().await;
    until("a session for each repository's scale set", || {
        let sets = rig.github.scale_sets();
        sets.len() == 2 && sets.iter().all(|s| s.has_session)
    })
    .await;
    // Polls that stand for seconds, as GitHub's do: only the Runner ends one early.
    rig.github.set_poll_timeout(Duration::from_secs(4));
    tokio::time::sleep(Duration::from_millis(500)).await;

    rig.queue().await;
    until("a machine for the first repository's job", || {
        rig.standing().len() == 1
    })
    .await;
    let runner = rig.standing().remove(0);
    rig.github.start_job(&runner);
    let others = rig
        .github
        .scale_sets()
        .into_iter()
        .find(|set| set.repo == OTHER)
        .expect("the other repository's scale set")
        .id;
    until("the other repository's poll to offer no slot any more", || {
        let polls = format!("GET /svc/_apis/runtime/runnerscalesets/{others}/messages");
        rig.github
            .requests()
            .iter()
            .rfind(|request| request.line.starts_with(&polls))
            .is_some_and(|poll| poll.max_capacity.as_deref() == Some("0"))
    })
    .await;
    let job = rig.github.queue_job(OTHER, &["self-hosted", "cf-2g"], "build");
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        rig.github.job_state(job),
        Some(JobState::Available),
        "the full Mac was assigned a job it cannot start"
    );
    assert_eq!(rig.machines.snapshot().waiting, 0);

    // Another Mac serves the same repository with the same labels and has room: the job is its.
    let elsewhere = ScaleSetClient::new(
        app(&rig.github, http(), rig.parts.clock.clone()),
        1,
        OTHER,
        &format!("{}/{OTHER}", rig.github.url()),
        Duration::from_secs(5),
    );
    let labels = ["self-hosted".to_string(), "cf-2g".to_string()];
    let set = elsewhere
        .ensure_scale_set("mac-c--cf-2g", &labels)
        .await
        .expect("a scale set");
    let session = elsewhere.open_session(set.id, "mac-c").await.expect("a session");
    rig.github.set_poll_timeout(Duration::from_millis(150));
    session.poll(0, 1).await.expect("a poll");
    session.poll(0, 1).await.expect("a poll");
    rig.github.github_looks();
    assert_eq!(rig.github.job_state(job), Some(JobState::Acquired(set.id)));
    assert_eq!(rig.standing().len(), 1, "and the full Mac started nothing for it");

    // Alone again, the first Mac still runs what is queued for it once its slot is free.
    let later = rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.github.job_state(later), Some(JobState::Available));
    rig.parts.vms.end(job_name(&runner), End::Ok);
    rig.github.complete_job(&runner, "succeeded");
    until("a machine for the job that waited at GitHub", || {
        rig.standing().iter().any(|standing| *standing != runner)
    })
    .await;
}

#[tokio::test]
async fn a_mac_whose_other_programs_leave_no_memory_for_what_it_serves_tells_the_console_it_is_closed_for_memory()
 {
    let rig = Rig::with_session().await;
    let status = |rig: &Rig| rig.report()["status"].clone();
    let said = status(&rig);
    assert_eq!((&said["open"], &said["reason"]), (&json!(true), &Value::Null));

    let short = rig.parts.settings.away.floor_bytes + (1 << 30);
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = short));
    let said = status(&rig);
    assert_eq!(
        (&said["open"], &said["reason"]),
        (&json!(false), &json!("memory"))
    );
    assert_eq!(said["scaleSets"][0]["capacity"], json!(0));
    rig.service.report().await;
    assert_eq!(
        rig.console.reports().last().expect("a report")["status"]["reason"],
        json!("memory"),
        "the console takes the report"
    );

    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = 64 << 30));
    let said = status(&rig);
    assert_eq!((&said["open"], &said["reason"]), (&json!(true), &Value::Null));
}

#[tokio::test]
async fn jobs_that_wait_while_nothing_runs_tell_the_console_what_keeps_the_first_of_them() {
    let rig = Rig::with_session().await;
    let idle = |rig: &Rig| {
        let said = rig.report()["status"].clone();
        (
            said["idleWithJobsWaitingSeconds"].clone(),
            said["idleWithJobsWaitingReason"].clone(),
        )
    };
    assert_eq!(
        idle(&rig),
        (Value::Null, Value::Null),
        "left out while nothing waits"
    );
    // GitHub assigned a job just before what else runs on the Mac took the memory: it waits, and nothing runs.
    let short = rig.parts.settings.away.floor_bytes + (1 << 30);
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = short));
    pass(&rig.machines, 1);
    let Claim::Waits { why, .. } = rig.machines.claim(cf_budget::Size::G2, None) else {
        panic!("the job got a machine");
    };
    assert_eq!(why, "memory");
    assert_eq!(idle(&rig).1, json!("nothing runs and no job starts: memory"));
}

/// One thing that happens to a Mac serving two repositories whose jobs take the same size. A number picks among
/// what is there at that moment.
#[derive(Debug, Clone)]
enum Happens {
    /// A job of the one repository or the other is queued.
    Queued(bool),
    /// An Actions runner that is up takes its job.
    Started(u8),
    /// A running job ends: GitHub hears it before its machine goes, or after.
    Completed(u8, bool),
    /// A job is cancelled, assigned here or waiting at GitHub.
    Cancelled(u8, bool),
    /// Every job that waits, at GitHub or assigned here, is cancelled: runs a newer push replaces, say.
    WaitingCancelled,
    /// An Actions runner of another Mac takes a job that waits at GitHub.
    TakenElsewhere(u8),
    /// What else runs on the Mac leaves room for no machine, or for all.
    Memory(bool),
    /// GitHub looks at its queue by itself, at a moment nobody can predict.
    GithubLooks,
    /// GitHub forgets a scale set's session.
    SessionLost(u8),
    /// A job starts here, ends, or both, and GitHub forgets every session before anyone read that: the messages
    /// go with them.
    LostWith(u8, Lost),
    /// A scale set's acknowledgements fail past every retry: its listener fails, and its session is closed and
    /// opened again.
    AckFails(u8),
    /// The Runner's process dies and starts again.
    Restart,
    /// The Runner dies at one of its next writes to its records, the write kept or lost with it, and starts again
    /// at the next step: between any two of its stored steps.
    Crash(u8, bool),
    /// GitHub's REST view of Actions runners answers one read behind, or as it is.
    RestStale(bool),
    /// GitHub's REST view of Actions runners fails this many times, past the HTTP retries at most twice.
    RestFails(u8),
    /// Five to eleven minutes pass for GitHub, the Runner and its machines: an assignment no Actions runner took is
    /// taken back, an idle Actions runner is surplus, and a machine that waits for a job past its idle time is
    /// stopped.
    Minutes(u8),
    /// GitHub fails every removal of an Actions runner, and keeps the registration, from now on while jobs come and
    /// go; or it removes them again.
    RemovalsFail(bool),
    /// Machines that start from now on boot until [`Happens::Up`] says they are up, or boot at once again: a job
    /// can start, and end, on an Actions runner whose machine the Runner has not seen up yet.
    SlowBoot(bool),
    /// One machine that boots says it is up.
    Up(u8),
    /// The Runner is uninstalled, whatever it is in the middle of. The last step.
    Uninstall,
    /// Tens of milliseconds pass.
    Wait(u8),
}

#[derive(Debug, Clone, Copy)]
enum Lost {
    Start,
    End,
    Both,
}

fn happens() -> impl Strategy<Value = Happens> {
    prop_oneof![
        6 => any::<bool>().prop_map(Happens::Queued),
        4 => any::<u8>().prop_map(Happens::Started),
        5 => (any::<u8>(), any::<bool>()).prop_map(|(i, first)| Happens::Completed(i, first)),
        4 => (any::<u8>(), any::<bool>()).prop_map(|(n, assigned)| Happens::Cancelled(n, assigned)),
        1 => any::<u8>().prop_map(Happens::TakenElsewhere),
        1 => Just(Happens::WaitingCancelled),
        2 => any::<bool>().prop_map(Happens::Memory),
        4 => Just(Happens::GithubLooks),
        1 => any::<u8>().prop_map(Happens::SessionLost),
        4 => (any::<u8>(), prop_oneof![Just(Lost::Start), Just(Lost::End), Just(Lost::Both)])
            .prop_map(|(i, lost)| Happens::LostWith(i, lost)),
        1 => any::<u8>().prop_map(Happens::AckFails),
        1 => Just(Happens::Restart),
        2 => (any::<u8>(), any::<bool>()).prop_map(|(at, kept)| Happens::Crash(at, kept)),
        2 => any::<bool>().prop_map(Happens::RestStale),
        2 => any::<u8>().prop_map(Happens::RestFails),
        3 => any::<u8>().prop_map(Happens::Minutes),
        3 => any::<bool>().prop_map(Happens::RemovalsFail),
        2 => any::<bool>().prop_map(Happens::SlowBoot),
        3 => any::<u8>().prop_map(Happens::Up),
        1 => Just(Happens::Uninstall),
        3 => (1u8..30).prop_map(Happens::Wait),
    ]
}

/// What moves an Actions runner's registration toward its removal, and what can go wrong on the way: the steps
/// that matter for whether a job that ran on a machine the Runner gives up is heard, without the ones that only
/// dilute them.
fn happens_around_removals() -> impl Strategy<Value = Happens> {
    prop_oneof![
        4 => any::<bool>().prop_map(Happens::Queued),
        4 => Just(Happens::GithubLooks),
        1 => any::<u8>().prop_map(Happens::Started),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(i, first)| Happens::Completed(i, first)),
        2 => any::<u8>().prop_map(|n| Happens::Cancelled(n, true)),
        2 => Just(Happens::WaitingCancelled),
        3 => any::<u8>().prop_map(Happens::Minutes),
        3 => any::<bool>().prop_map(Happens::RemovalsFail),
        3 => (any::<u8>(), prop_oneof![Just(Lost::Start), Just(Lost::End), Just(Lost::Both)])
            .prop_map(|(i, lost)| Happens::LostWith(i, lost)),
        1 => any::<bool>().prop_map(Happens::RestStale),
        1 => any::<u8>().prop_map(Happens::RestFails),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(at, kept)| Happens::Crash(at, kept)),
        1 => any::<bool>().prop_map(Happens::SlowBoot),
        2 => any::<u8>().prop_map(Happens::Up),
        2 => (1u8..30).prop_map(Happens::Wait),
    ]
}

/// How soon, in these tests' timings, the Mac offers room for a job it serves that waits while a machine of its size
/// fits: a session GitHub forgot opens again (the backoff here waits 100 ms at most), and two polls show the room (the
/// fake GitHub answers each within 150 ms).
const OFFERS_WITHIN: Duration = Duration::from_secs(3);

/// How soon GitHub's look at its queue, while the Mac offers room, moves a job this Mac serves: the assignment and the
/// start follow.
const STARTS_WITHIN: Duration = Duration::from_secs(3);

/// How soon a job whose start and end went with a session is at the console once its machine is gone: the wait for
/// GitHub's word ([`cf_daemon::RESULT_WAIT`], two minutes of the Mac's clock, which the rig runs at a minute a
/// second), and a sweep.
const HEARD_WITHIN: Duration = Duration::from_secs(10);

/// Failed acknowledgements that outlast the HTTP retries ([`fast`]: two) and fail the listener once.
const PAST_RETRIES: usize = 3;

const SECOND: &str = "org/second";

/// What GitHub's removal of an Actions runner is asked at.
const REMOVAL: &str = "distributedtask/pools/0/agents/";

/// The Runner died, or dies: it starts again, and opens every scale set's session anew. Says which registrations
/// GitHub removed at the dead Runner's request without that Runner having written the answer down: it died between
/// GitHub's 204 and its write.
async fn start_again(rig: &mut Rig, sets: &[i64], causes: &mut [usize]) -> Vec<String> {
    let mut lost = Vec::new();
    rig.restart_after(|rig| {
        let kept: BTreeSet<String> = rig
            .parts
            .store
            .load()
            .into_iter()
            .filter(|record| !record.unregistered)
            .map(|record| record.runner)
            .collect();
        lost = rig
            .github
            .removed_at_request()
            .into_iter()
            .filter(|runner| kept.contains(runner))
            .collect();
    })
    .await;
    for (at, set) in sets.iter().enumerate() {
        rig.github.lose_session(*set);
        causes[at] += 1;
    }
    rig.service.discover().await;
    lost
}

/// What is stored of a machine never disagrees with itself, at any write in any phase, whenever the Runner dies:
/// what GitHub said of the job and the lifecycle agree ([`cf_daemon::Record::agrees`]).
fn stored_facts_agree(rig: &Rig) -> Result<(), TestCaseError> {
    let disagreements = rig.store.disagreements();
    prop_assert!(
        disagreements.is_empty(),
        "written with facts that disagree: {disagreements:?}"
    );
    Ok(())
}

/// The Runner never stops the machine of a job that runs as if nobody needed it: a job's machine goes when the job
/// ends, or for a cause (memory, disk, its time, the Mac's sleep).
fn no_running_job_stopped(rig: &Rig, jobs: &[i64]) -> Result<(), TestCaseError> {
    let standing = rig.standing();
    let stopped = rig.parts.vms.stopped();
    for runner in running(rig, jobs) {
        let name = job_name(&runner);
        let cancelled = stopped
            .iter()
            .any(|(job, reason, _)| *job == name && *reason == StopReason::Cancelled);
        let for_cause = stopped
            .iter()
            .any(|(job, reason, _)| *job == name && *reason != StopReason::Cancelled);
        prop_assert!(
            standing.contains(&runner) || !cancelled || for_cause,
            "the machine of {runner}, whose job runs, was stopped as unneeded"
        );
    }
    Ok(())
}

/// Machines started may never outnumber the jobs GitHub handed to this Mac, each hand-out counted, and the machines
/// that never came up, each of which leaves its job to one that replaces it.
fn one_machine_per_assignment(rig: &Rig) -> Result<(), TestCaseError> {
    let (started, assigned) = (rig.parts.vms.started().len(), rig.github.assignments());
    // A machine that never came up leaves its job to a machine that replaces it.
    let failed = rig.parts.vms.died_booting();
    prop_assert!(
        started <= assigned + failed,
        "{started} machines for {assigned} jobs handed to this Mac, {failed} of them never up"
    );
    Ok(())
}

async fn live(slots: u32, script: Vec<Happens>) -> Result<(), TestCaseError> {
    let mut rig = Rig::new(slots).await;
    rig.github.add_repo(SECOND, true);
    rig.github.set_workflow(SECOND, "ci.yml", WORKFLOW);
    rig.service.discover().await;
    until("a session for each repository's scale set", || {
        let sets = rig.github.scale_sets();
        sets.len() == 2 && sets.iter().all(|s| s.has_session)
    })
    .await;
    let roomy = 64u64 << 30;
    let short = rig.parts.settings.away.floor_bytes + (1 << 30);
    let sets: Vec<i64> = rig.github.scale_sets().iter().map(|s| s.id).collect();
    let mut jobs: Vec<i64> = vec![];
    let mut uninstall = false;
    // The Actions runners that ran a job, and for each scale set what may open its session again.
    let mut ran = BTreeSet::new();
    let mut causes = vec![1usize; sets.len()];
    // Registrations GitHub removed at the Runner's request whose answer died with a Runner.
    let mut lost = BTreeSet::new();
    for step in script {
        if rig.store.crashed() {
            lost.extend(start_again(&mut rig, &sets, &mut causes).await);
        }
        let running = |jobs: &[i64]| -> Vec<String> {
            rig.standing()
                .into_iter()
                .filter(|runner| {
                    jobs.iter()
                        .any(|&job| rig.github.job_state(job) == Some(JobState::Running(runner.clone())))
                })
                .collect()
        };
        let idle = |jobs: &[i64]| -> Vec<String> {
            let busy = running(jobs);
            rig.standing().into_iter().filter(|r| !busy.contains(r)).collect()
        };
        let in_state = |wanted: fn(&JobState) -> bool| -> Vec<i64> {
            jobs.iter()
                .copied()
                .filter(|&job| rig.github.job_state(job).as_ref().is_some_and(wanted))
                .collect()
        };
        let start = |runner: &str, ran: &mut BTreeSet<String>| {
            // An Actions runner runs only in a guest that is up: the guest says so to its vm-host before it starts
            // the workload, whether the Runner looked since or not.
            if rig.booting().iter().any(|booting| booting == runner) {
                rig.parts.vms.ready(job_name(runner));
            }
            rig.github.start_job(runner);
            if running(&jobs).iter().any(|r| r == runner) {
                ran.insert(runner.to_string());
            }
        };
        match step {
            Happens::Queued(first) => {
                let repo = if first { REPO } else { SECOND };
                jobs.push(rig.github.queue_job(repo, &["self-hosted", "cf-2g"], "build"));
            }
            Happens::Started(n) => {
                if let Some(runner) = pick(&idle(&jobs), n) {
                    start(&runner, &mut ran);
                }
            }
            Happens::Completed(n, github_first) => {
                if let Some(runner) = pick(&running(&jobs), n) {
                    if github_first {
                        // GitHub's word may come before the Runner saw the machine up.
                        rig.github.complete_job(&runner, "succeeded");
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        rig.parts.vms.end(job_name(&runner), End::Ok);
                    } else {
                        rig.parts.vms.end(job_name(&runner), End::Ok);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        rig.github.complete_job(&runner, "succeeded");
                    }
                }
            }
            Happens::Cancelled(n, assigned) => {
                let waiting = if assigned {
                    in_state(|s| matches!(s, JobState::Acquired(_)))
                } else {
                    in_state(|s| *s == JobState::Available)
                };
                if let Some(job) = pick(&waiting, n) {
                    rig.github.cancel_job(job);
                }
            }
            Happens::WaitingCancelled => {
                for job in in_state(|s| matches!(s, JobState::Available | JobState::Acquired(_))) {
                    rig.github.cancel_job(job);
                }
            }
            Happens::TakenElsewhere(n) => {
                if let Some(job) = pick(&in_state(|s| *s == JobState::Available), n) {
                    rig.github.take_job(job);
                }
            }
            Happens::Memory(room) => {
                let bytes = if room { roomy } else { short };
                rig.machines
                    .with_host(|host| host.set(|reading| reading.available_bytes = bytes));
            }
            Happens::GithubLooks => rig.github.github_looks(),
            Happens::SessionLost(n) => {
                if let Some(at) = pick(&(0..sets.len()).collect::<Vec<_>>(), n) {
                    rig.github.lose_session(sets[at]);
                    causes[at] += 1;
                }
            }
            Happens::LostWith(n, lost) => {
                let runner = match lost {
                    Lost::End => pick(&running(&jobs), n),
                    Lost::Start | Lost::Both => pick(&idle(&jobs), n),
                };
                if let Some(runner) = runner {
                    if matches!(lost, Lost::Start | Lost::Both) {
                        start(&runner, &mut ran);
                    }
                    if matches!(lost, Lost::End | Lost::Both) {
                        rig.github.complete_job(&runner, "succeeded");
                        rig.parts.vms.end(job_name(&runner), End::Ok);
                    }
                    for (at, set) in sets.iter().enumerate() {
                        rig.github.lose_session(*set);
                        causes[at] += 1;
                    }
                }
            }
            Happens::AckFails(n) => {
                if let Some(at) = pick(&(0..sets.len()).collect::<Vec<_>>(), n) {
                    rig.github.fail(
                        &format!("DELETE /svc/_apis/runtime/runnerscalesets/{}/messages/", sets[at]),
                        Reply::status_json(500, &json!({"message": "try again"})),
                        PAST_RETRIES,
                    );
                    causes[at] += 1;
                }
            }
            Happens::Restart => lost.extend(start_again(&mut rig, &sets, &mut causes).await),
            Happens::Crash(at, kept) => rig.crash_at(1 + usize::from(at % 8), kept),
            Happens::SlowBoot(slow) => rig.parts.vms.slow_boot(slow),
            Happens::Up(n) => {
                if let Some(runner) = pick(&rig.booting(), n) {
                    rig.parts.vms.ready(job_name(&runner));
                }
            }
            Happens::RestStale(stale) => rig.github.set_rest_stale(stale),
            Happens::RestFails(times) => {
                for repo in [REPO, SECOND] {
                    rig.github.fail(
                        &format!("GET /repos/{repo}/actions/runners/"),
                        Reply::status_json(502, &json!({"message": "bad gateway"})),
                        1 + usize::from(times % 6),
                    );
                }
            }
            Happens::Minutes(n) => {
                let time = Duration::from_secs(60 * (5 + u64::from(n % 7)) + 1);
                rig.github.pass(time);
                rig.later(time);
                pass(&rig.machines, time.as_secs());
            }
            Happens::RemovalsFail(true) => rig
                .github
                .fail_until_recovered(REMOVAL, Reply::status_json(500, &json!({"message": "try again"}))),
            Happens::RemovalsFail(false) => rig.github.recover(REMOVAL),
            Happens::Uninstall => uninstall = true,
            Happens::Wait(tens) => {
                tokio::time::sleep(Duration::from_millis(u64::from(tens) * 10)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        one_machine_per_assignment(&rig)?;
        no_running_job_stopped(&rig, &jobs)?;
        stored_facts_agree(&rig)?;
        let opens = session_opens(&rig);
        prop_assert!(
            opens.iter().zip(&causes).all(|(opened, may)| opened <= may),
            "sessions opened {opens:?} times, with cause for {causes:?}"
        );
        if uninstall {
            break;
        }
    }
    if rig.store.crashed() {
        lost.extend(start_again(&mut rig, &sets, &mut causes).await);
    }
    // GitHub removes Actions runners again, and a crash that has not come does not come now.
    rig.github.recover(REMOVAL);
    rig.store.revive();

    if uninstall {
        // GitHub ends the jobs of Actions runners that went away, in its own time.
        let github = rig.github.clone();
        let all = jobs.clone();
        let ends = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            for job in all {
                if let Some(JobState::Running(runner)) = github.job_state(job) {
                    github.complete_job(&runner, "failed");
                }
            }
        });
        let left = rig.service.retire(Duration::from_secs(8), false).await;
        ends.await.expect("GitHub ends the jobs");
        prop_assert!(left.is_empty(), "the uninstall left {left:?}");
        prop_assert!(
            rig.github.runners().is_empty(),
            "registrations left: {:?}",
            rig.github.runners()
        );
        prop_assert!(
            rig.parts.store.load().is_empty(),
            "records left: {:?}",
            rig.parts.store.load()
        );
        all_heard(&rig, &ran, &lost).await?;
        rig.stop_pump().await;
        rig.service.kill();
        return Ok(());
    }

    // Room for every machine: what waits at GitHub moves at each of GitHub's looks, until the slots are full.
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = roomy));
    // Machines boot again, and the start breaker that machines which never came up opened lets the next try out.
    rig.parts.vms.slow_boot(false);
    if rig.parts.vms.died_booting() > 0 {
        let time = Duration::from_secs(16 * 60);
        rig.github.pass(time);
        rig.later(time);
        pass(&rig.machines, time.as_secs());
    }
    let slots = usize::try_from(slots).unwrap_or(usize::MAX);
    let waiting = |rig: &Rig| {
        jobs.iter()
            .filter(|&&job| rig.github.job_state(job) == Some(JobState::Available))
            .count()
    };
    let offered = |rig: &Rig| jobs.iter().any(|&job| rig.github.offers_room_for(job));
    loop {
        let (before, standing) = (waiting(&rig), rig.standing().len());
        if before == 0 || standing >= slots {
            break;
        }
        // The Mac's polls show the room for a waiting job, once a session GitHub forgot is open again; then GitHub
        // looks. A look before that hands nothing out, and GitHub looks again only at its own moments. A machine that
        // stands up meanwhile for a job assigned before may take the last slot: then the room is looked at anew.
        let free = std::time::Instant::now();
        while !offered(&rig) && (waiting(&rig), rig.standing().len()) == (before, standing) {
            prop_assert!(
                free.elapsed() < OFFERS_WITHIN,
                "{before} jobs wait at GitHub and {standing} of {slots} machines stand, and the Mac offered no room \
                 for them in {:?}",
                free.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if (waiting(&rig), rig.standing().len()) != (before, standing) {
            continue;
        }
        rig.github.github_looks();
        let looked = std::time::Instant::now();
        while waiting(&rig) == before && rig.standing().len() == standing {
            prop_assert!(
                looked.elapsed() < STARTS_WITHIN,
                "{before} jobs wait at GitHub and {standing} of {slots} machines stand, {:?} after GitHub looked",
                looked.elapsed()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    one_machine_per_assignment(&rig)?;

    // Every job that ran here is at the console, started and ended, however many of GitHub's messages were lost.
    for runner in running(&rig, &jobs) {
        rig.github.complete_job(&runner, "succeeded");
        rig.parts.vms.end(job_name(&runner), End::Ok);
    }
    all_heard(&rig, &ran, &lost).await?;
    rig.stop_pump().await;
    rig.service.kill();
    Ok(())
}

/// Every job that ran here reaches the console, its start and its end, each once. A start the Runner reported on
/// its own (GitHub's answer implied the job, or the registration went before a crash) has its end only once the
/// machine is gone and the wait for GitHub's word is over: that end is waited for too.
async fn all_heard(rig: &Rig, ran: &BTreeSet<String>, lost: &BTreeSet<String>) -> Result<(), TestCaseError> {
    let heard_from = std::time::Instant::now();
    loop {
        let unheard: Vec<&String> = ran
            .iter()
            .filter(|runner| heard(rig, runner) != (true, true))
            .collect();
        let events = rig.events();
        let unended: Vec<&str> = events
            .iter()
            .filter_map(|(id, _)| id.strip_suffix("/started"))
            .filter(|runner| {
                !events
                    .iter()
                    .any(|(id, end)| *id == format!("{runner}/completed") && !end.is_null())
            })
            .collect();
        if unheard.is_empty() && unended.is_empty() {
            break;
        }
        prop_assert!(
            heard_from.elapsed() < HEARD_WITHIN,
            "jobs that ran here and never reached the console: {unheard:?}; started and never ended: {unended:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let events = rig.events();
    for (id, _) in &events {
        prop_assert_eq!(
            events.iter().filter(|(other, _)| other == id).count(),
            1,
            "{} reached the console twice",
            id
        );
    }
    let started: BTreeSet<&str> = events
        .iter()
        .filter_map(|(id, _)| id.strip_suffix("/started"))
        .collect();
    let ended: BTreeSet<&str> = events
        .iter()
        .filter_map(|(id, _)| id.strip_suffix("/completed"))
        .collect();
    prop_assert_eq!(
        &started,
        &ended,
        "a start without its end, or an end without its start"
    );
    // Accepted, and only this: GitHub removed a registration at the Runner's request and the Runner died before it
    // wrote that answer down. The next Runner finds it gone with no word that no job ran there and reports a job
    // (`docs/job-state.md`): one too many, never one lost.
    for runner in &started {
        prop_assert!(
            ran.contains(*runner) || lost.contains(*runner),
            "{} never ran a job, yet the console heard of one",
            runner
        );
    }
    Ok(())
}

/// The Actions runners whose job runs at GitHub.
fn running(rig: &Rig, jobs: &[i64]) -> Vec<String> {
    jobs.iter()
        .filter_map(|&job| match rig.github.job_state(job) {
            Some(JobState::Running(runner)) => Some(runner),
            _ => None,
        })
        .collect()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    /// Whatever happens in whatever order, over long runs: no job gets two machines; a session opens again only
    /// when GitHub forgot it, it failed, or the Runner started again, so no state makes the Runner open sessions
    /// over and over, a job GitHub holds for ever included; each of GitHub's looks moves a job this Mac serves that
    /// waits while a machine fits; and every job that ran here reaches the console with its start and its end
    /// however many of GitHub's messages a session took with it.
    #[test]
    fn whatever_happens_no_job_gets_two_sessions_open_only_for_a_cause_and_every_job_that_ran_is_heard(
        slots in 1u32..=2,
        script in proptest::collection::vec(happens(), 1..40),
    ) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(live(slots, script))?;
    }

    /// The same, for what happens around the removal of an Actions runner's registration: a removal GitHub fails
    /// or answers late, a job that comes to an Actions runner the Runner meant to remove, its messages lost, the
    /// Runner dying between its writes.
    #[test]
    fn around_removals_every_job_that_ran_is_heard_and_no_running_job_is_stopped(
        script in proptest::collection::vec(happens_around_removals(), 1..40),
    ) {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime")
            .block_on(live(2, script))?;
    }
}

/// How often each scale set's session was opened.
fn session_opens(rig: &Rig) -> Vec<usize> {
    rig.github
        .scale_sets()
        .iter()
        .map(|set| {
            let line = format!("POST /svc/_apis/runtime/runnerscalesets/{}/sessions", set.id);
            rig.github.requests().iter().filter(|r| r.line == line).count()
        })
        .collect()
}

/// A session closes after a failure while GitHub's queue holds the start and the end of a job that ran here, and
/// the queue goes with the session. What GitHub knows of the Actions runner says the job ran: the console hears of
/// its start and its end all the same, once the wait for GitHub's word is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_whose_start_and_end_went_with_a_closed_session_still_reaches_the_console() {
    job_lost_with_a_session_is_heard(false).await;
}

/// The same, and the Runner starts again after the machine went: the one that starts next still asks GitHub before
/// it removes the registration.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_whose_start_and_end_went_with_a_closed_session_is_heard_by_the_runner_that_starts_next() {
    job_lost_with_a_session_is_heard(true).await;
}

async fn job_lost_with_a_session_is_heard(restart: bool) {
    let mut rig = Rig::new(1).await;
    rig.service.discover().await;
    until("the scale set's session", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    let set = rig.github.scale_sets()[0].id;
    tokio::time::sleep(Duration::from_millis(400)).await;
    rig.github.fail(
        &format!("DELETE /svc/_apis/runtime/runnerscalesets/{set}/messages/"),
        Reply::status_json(500, &json!({"message": "try again"})),
        10,
    );
    let ran = Arc::new(Mutex::new(None));
    rig.github.before(
        &format!("DELETE /svc/_apis/runtime/runnerscalesets/{set}/sessions/"),
        {
            let ran = ran.clone();
            move |github| {
                let runner = github.runners().pop().expect("a registered Actions runner");
                github.start_job(&runner);
                github.complete_job(&runner, "succeeded");
                *ran.lock().unwrap() = Some(runner);
            }
        },
    );
    rig.queue().await;
    until("the job to run and end while the session closed", || {
        ran.lock().unwrap().is_some()
    })
    .await;
    let runner = ran.lock().unwrap().clone().expect("runner");
    rig.parts.vms.end(job_name(&runner), End::Ok);
    if restart {
        until("the machine to be gone", || rig.standing().is_empty()).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        rig.restart().await;
        rig.github.lose_session(set);
        rig.service.discover().await;
    }
    until("the job's start and end at the console", || {
        let events = rig.events();
        events.iter().any(|(id, _)| *id == format!("{runner}/started"))
            && events
                .iter()
                .any(|(id, end)| *id == format!("{runner}/completed") && !end.is_null())
    })
    .await;
}

/// A job's start goes with a session GitHub forgets, and its machine is gone before a session opens again. GitHub's
/// counts still name the job as assigned, and nothing of the Runner's says a machine had it: no machine is started
/// for a job that is already over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_that_went_with_its_start_unheard_gets_no_second_machine_from_githubs_counts() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    rig.github.fail(
        &format!("POST /svc/_apis/runtime/runnerscalesets/{set}/sessions"),
        Reply::status_json(503, &json!({"message": "busy"})),
        6,
    );
    rig.github.start_job(&runner);
    rig.github.lose_session(set);
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the machine to be gone", || rig.standing().is_empty()).await;
    until("a session again", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(rig.parts.vms.started().len(), 1, "the job had its machine");
}

/// GitHub holds a job back and never looks again: nothing the Runner does makes it, so nothing is tried over and
/// over. Each scale set's session opens once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_github_holds_back_for_ever_opens_no_session_again() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    until("the scale set's session", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    let short = rig.parts.settings.away.floor_bytes + (1 << 30);
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = short));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let held = rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    tokio::time::sleep(Duration::from_millis(300)).await;
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = 64 << 30));
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(rig.github.job_state(held), Some(JobState::Available));
    assert_eq!(session_opens(&rig), vec![1]);
}

/// Whether the console has the job's start, and its end, of this Actions runner.
fn heard(rig: &Rig, runner: &str) -> (bool, bool) {
    let events = rig.events();
    (
        events.iter().any(|(id, _)| *id == format!("{runner}/started")),
        events
            .iter()
            .any(|(id, end)| *id == format!("{runner}/completed") && !end.is_null()),
    )
}

/// Keeps the scale set's session from opening again, as a GitHub that is away for a while does.
fn no_session_for_a_while(rig: &Rig, set: i64) {
    rig.github.fail(
        &format!("POST /svc/_apis/runtime/runnerscalesets/{set}/sessions"),
        Reply::status_json(503, &json!({"message": "busy"})),
        10_000,
    );
}

/// Ends the job of `runner` just before GitHub answers the first `poll` that comes while a session is open.
fn end_at_first_poll_with_a_session(github: &FakeGithub, poll: String, runner: String) {
    github.before(&poll.clone(), move |github| {
        if github.scale_sets().iter().any(|s| s.has_session) {
            github.complete_job(&runner, "succeeded");
        } else {
            end_at_first_poll_with_a_session(github, poll, runner);
        }
    });
}

/// GPT, scaler.rs:525: a session opens with counts that name a running job, and the job ends right after them: at
/// the Runner's first poll once the session is open (its start was heard, so the REST view is not asked). The counts
/// are from before the end: no second machine starts for a job that is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_that_ends_as_a_session_opens_gets_no_second_machine_from_the_opening_counts() {
    // The second time the session is lost as a poll fails: the Runner polls once more before it opens another.
    for polls_again in [false, true] {
        let rig = Rig::new(2).await;
        rig.service.discover().await;
        let runner = rig.job_running().await;
        until("the start at the console", || heard(&rig, &runner).0).await;
        let set = rig.github.scale_sets()[0].id;
        let poll = format!("GET /svc/_apis/runtime/runnerscalesets/{set}/messages");
        if polls_again {
            let failed = poll.clone();
            rig.github.before(&poll, move |github| {
                github.lose_session(set);
                github.fail(&failed, Reply::text(503, "unavailable"), 1);
            });
        }
        end_at_first_poll_with_a_session(&rig.github, poll, runner.clone());
        if !polls_again {
            rig.github.lose_session(set);
        }
        until("the job over at GitHub", || {
            !rig.github.runners().contains(&runner)
        })
        .await;
        until("a session again", || {
            rig.github.scale_sets().iter().any(|s| s.has_session)
        })
        .await;
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(rig.parts.vms.started().len(), 1, "one job, one machine");
    }
}

/// GPT, scaler.rs:582: a job is cancelled before its Actions runner took it, the surplus Actions runner is removed
/// at GitHub, and the Runner dies before the machine is gone. The Runner that starts next finds GitHub's answer to
/// that removal written down: it stops the machine without asking again, and takes nothing for a job.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surplus_removed_just_before_the_runner_died_is_not_taken_for_a_job_by_the_next() {
    let mut rig = Rig::new(1).await;
    rig.service.discover().await;
    let job = rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    rig.parts.vms.ignore_stops(true);
    rig.parts.vms.unkillable(job_name(&runner), true);
    rig.github.cancel_job(job);
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    until("the surplus registration removed", || {
        rig.github.runners().is_empty()
    })
    .await;
    let removals = |rig: &Rig| {
        rig.github
            .requests()
            .iter()
            .filter(|r| r.line.starts_with("DELETE") && r.line.contains("distributedtask/pools/0/agents/"))
            .count()
    };
    let stops = |rig: &Rig| {
        rig.parts
            .vms
            .stopped()
            .iter()
            .filter(|(job, ..)| *job == job_name(&runner))
            .count()
    };
    until("the Runner's own stop, which the machine ignores", || {
        stops(&rig) > 0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (removed_before, stopped_before) = (removals(&rig), stops(&rig));
    // As a Runner of rules 1 could leave it, which wrote `unregistered` only on GitHub's answer: the registration
    // written gone, and its stop not yet.
    rig.restart_after(|rig| {
        for mut record in rig.parts.store.load() {
            record.rules = 1;
            if let Some(slot) = record.slot.as_mut().filter(|_| record.unregistered) {
                let mut saved = slot.saved();
                saved.phase = cf_lifecycle::Phase::Idle;
                *slot = saved.into();
            }
            rig.parts.store.save(&record);
        }
    })
    .await;
    let set = rig.github.scale_sets()[0].id;
    rig.github.lose_session(set);
    rig.service.discover().await;
    until("a session again", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    until("the machine stopped by the next Runner", || {
        stops(&rig) > stopped_before
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        removals(&rig),
        removed_before,
        "GitHub's answer was written down: nothing to remove again"
    );
    assert_eq!(heard(&rig, &runner), (false, false), "no job ran there");
    rig.parts.vms.ignore_stops(false);
    rig.parts.vms.unkillable(job_name(&runner), false);
    until("the surplus machine gone", || rig.standing().is_empty()).await;
    until("no record", || rig.parts.store.load().is_empty()).await;
    assert_eq!(heard(&rig, &runner), (false, false), "no job ran there");
}

/// GPT, service.rs:764: a job's start and end go with a session, its machine ends, and the REST view answers one
/// read behind: idle, as registered. The registration is gone when the Runner removes it. The job ran, and the
/// console hears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stale_idle_answer_does_not_hide_a_job_whose_messages_were_lost() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    rig.github.set_rest_stale(true);
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    rig.github.complete_job(&runner, "succeeded");
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the start and end at the console", || {
        heard(&rig, &runner) == (true, true)
    })
    .await;
}

/// GPT, machines.rs:968: a job's start and end go with a session, its machine ends, and an uninstall begins before
/// the Runner asked GitHub, whose REST view fails for a moment. The start the REST view then shows brings its end
/// with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uninstall_that_finds_a_lost_job_reports_its_end_as_well_as_its_start() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    rig.github.complete_job(&runner, "succeeded");
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the machine to be gone", || rig.standing().is_empty()).await;
    rig.github.fail(
        "GET /repos/org/app/actions/runners/",
        Reply::status_json(502, &json!({"message": "bad gateway"})),
        PAST_RETRIES,
    );
    let left = rig.service.retire(Duration::from_secs(6), false).await;
    assert_eq!(left, Vec::<String>::new());
    assert_eq!(heard(&rig, &runner), (true, true), "{:?}", rig.events());
}

/// A job is cancelled before its Actions runner took it, and the surplus machine is removed and gone. Its guest came
/// up and GitHub never said a word of a job, but the Runner removed it itself: GitHub is not asked about it once the
/// wait is over, since its 404 is the Runner's own removal, and no job reaches the console.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surplus_removed_and_gone_is_never_taken_for_a_job_that_went_unheard() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let job = rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    rig.github.cancel_job(job);
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    until("the surplus machine gone", || rig.standing().is_empty()).await;
    rig.later(RESULT_WAIT + Duration::from_secs(1));
    until("no record", || rig.parts.store.load().is_empty()).await;
    assert_eq!(heard(&rig, &runner), (false, false), "no job ran there");
}

/// A job's start and end go with a session and its machine ends. The REST view says idle twice, wrongly, and
/// GitHub refuses to remove the Actions runner, since it still holds a job there: that refusal is a job, and the
/// console hears its start and its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_removal_github_refuses_for_a_job_is_a_job_whatever_the_rest_view_said() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    rig.github.fail(
        "GET /repos/org/app/actions/runners/",
        Reply::status_json(
            200,
            &json!({"id": 1, "name": runner, "busy": false, "status": "offline"}),
        ),
        10_000,
    );
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the machine to be gone", || rig.standing().is_empty()).await;
    for _ in 0..3 {
        rig.later(RESULT_WAIT + Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    until("the start and end at the console", || {
        heard(&rig, &runner) == (true, true)
    })
    .await;
}

/// A surplus Actions runner takes a job between the Runner's look at the REST view and its removal, and GitHub
/// refuses the removal. The job is the Runner's to hear: its start reaches the console, a Runner that starts next
/// leaves the machine to its job, and its end follows when the machine is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surplus_that_takes_a_job_as_it_is_removed_keeps_its_machine_and_its_job() {
    let mut rig = Rig::new(1).await;
    rig.service.discover().await;
    let job = rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    rig.github.cancel_job(job);
    rig.github.fail(
        "distributedtask/pools/0/agents/",
        Reply::status_json(422, &json!({"message": "the runner still has a job"})),
        1,
    );
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    until("the start at the console", || heard(&rig, &runner).0).await;
    rig.restart().await;
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(rig.standing(), vec![runner.clone()], "the machine keeps its job");
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the machine to be gone", || rig.standing().is_empty()).await;
    for _ in 0..3 {
        rig.later(RESULT_WAIT + Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    until("the end at the console", || heard(&rig, &runner) == (true, true)).await;
}

/// As above, while the Runner goes on: the job GitHub's refusal named is dated when it was heard, and its
/// Actions runner is busy from then on, so the scaler does not take it for a surplus again and ask for its removal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surplus_github_refuses_for_a_job_is_busy_from_then_on_and_its_start_dated_when_heard() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let job = rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    rig.github.cancel_job(job);
    rig.github.fail(
        REMOVAL,
        Reply::status_json(422, &json!({"message": "the runner still has a job"})),
        1,
    );
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    until("the start at the console", || heard(&rig, &runner).0).await;
    let started = rig
        .console
        .job_events()
        .into_iter()
        .find(|event| event["id"] == format!("{runner}/started"))
        .expect("the start");
    assert!(
        !started["at"].as_str().unwrap_or("1970").starts_with("1970"),
        "dated when GitHub said so: {started}"
    );
    assert!(
        rig.parts.roster.get(&runner).is_some_and(|r| r.busy),
        "the Actions runner has a job"
    );
    let removals = rig.github.count(REMOVAL);
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        rig.github.count(REMOVAL),
        removals,
        "a job runs there: it is no surplus"
    );
    assert_eq!(rig.standing(), vec![runner], "the machine keeps its job");
}

/// A job's start goes with a session GitHub forgets, and its machine goes on with the job. When a session opens
/// again, the console hears the start while the job runs, not only once its machine is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_lost_with_a_session_is_heard_when_the_next_one_opens() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    rig.github.fail(
        &format!("POST /svc/_apis/runtime/runnerscalesets/{set}/sessions"),
        Reply::status_json(503, &json!({"message": "busy"})),
        2,
    );
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    until("a session again", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    until("the start at the console", || heard(&rig, &runner).0).await;
    assert_eq!(rig.standing(), vec![runner], "heard while the job runs");
}

/// Sol, machines.rs:1614: the Runner dies as it writes GitHub's word that a job started. That word and what it does to
/// the machine's lifecycle are one write, and a record an older Runner left with the two apart (the job started, the
/// lifecycle idle) is read by its one fact: the job runs. The machine is not stopped as idle while its job runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_that_dies_as_it_hears_a_start_leaves_the_job_its_machine() {
    let mut rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    until("the machine up and waiting", || {
        rig.parts.store.load().iter().any(|r| {
            r.slot
                .as_ref()
                .is_some_and(|s| s.saved().phase == cf_lifecycle::Phase::Idle)
        })
    })
    .await;
    rig.crash_at(1, true);
    rig.github.start_job(&runner);
    until("the Runner to die at its write", || rig.store.crashed()).await;
    let stored = rig.parts.store.load();
    assert!(
        stored.iter().any(|r| r.github.is_some()
            && r.slot
                .as_ref()
                .is_some_and(|s| s.saved().phase == cf_lifecycle::Phase::Running)),
        "the write that was kept: {stored:?}"
    );
    // What a Runner from before this rule could have left: the job's start, and a lifecycle still idle. GitHub's REST
    // view is away and the session's messages are lost, so only the record can say the job runs.
    let set = rig.github.scale_sets()[0].id;
    rig.github.lose_session(set);
    rig.github.fail_until_recovered(
        "GET /repos/org/app/actions/runners/",
        Reply::status_json(502, &json!({"message": "bad gateway"})),
    );
    rig.restart_after(|rig| {
        for mut record in stored {
            record.rules = 0;
            if let Some(slot) = record.slot.as_mut() {
                let mut saved = slot.saved();
                saved.phase = cf_lifecycle::Phase::Idle;
                *slot = saved.into();
            }
            rig.parts.store.save(&record);
        }
    })
    .await;
    until("the job's start read as a running job", || {
        rig.parts.store.load().iter().any(|r| {
            r.slot
                .as_ref()
                .is_some_and(|s| s.saved().phase == cf_lifecycle::Phase::Running)
        })
    })
    .await;
    pass(&rig.machines, 11 * 60);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.standing(),
        vec![runner.clone()],
        "the machine of a running job stands"
    );
    assert!(
        !rig.parts
            .vms
            .stopped()
            .iter()
            .any(|(job, ..)| *job == job_name(&runner)),
        "stops: {:?}",
        rig.parts.vms.stopped()
    );
}

/// Found by the generated sequences: a job's start goes with a session and the machine waits past its idle limit.
/// The Runner believes it idle, but does not stop it on that belief: it removes the registration first, GitHub
/// refuses for the job that runs there, and the job is heard and runs to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_machine_past_its_idle_limit_whose_start_was_lost_keeps_its_job() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    pass(&rig.machines, 11 * 60);
    until("GitHub's refusal heard as the job's start", || {
        heard(&rig, &runner).0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.standing(),
        vec![runner.clone()],
        "the machine of a running job stands"
    );
    assert!(
        !rig.parts
            .vms
            .stopped()
            .iter()
            .any(|(job, ..)| *job == job_name(&runner)),
        "stops: {:?}",
        rig.parts.vms.stopped()
    );
    let removals = rig.github.count(REMOVAL);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(
        rig.github.count(REMOVAL),
        removals,
        "a job runs there: its removal is not asked for again"
    );
    rig.github.complete_job(&runner, "succeeded");
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the job's end at the console", || {
        heard(&rig, &runner) == (true, true)
    })
    .await;
}

/// As below, but the job is over and its machine still stands when GitHub takes removals again: the registration
/// is gone, the Actions runner removed it itself. That 404 is the job's, and the console hears it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surplus_found_gone_while_its_machine_stands_ran_a_job() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let job = rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    rig.github.cancel_job(job);
    rig.github
        .fail_until_recovered(REMOVAL, Reply::status_json(500, &json!({"message": "try again"})));
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    until("the removal to fail", || {
        rig.github.count(REMOVAL) >= PAST_RETRIES
    })
    .await;
    let next = rig.queue().await;
    until("the Actions runner to be needed again", || {
        rig.github.job_state(next) == Some(JobState::Acquired(rig.github.scale_sets()[0].id))
    })
    .await;
    let set = rig.github.scale_sets()[0].id;
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    rig.github.complete_job(&runner, "succeeded");
    assert!(
        !rig.github.runners().contains(&runner),
        "a one-job Actions runner removes its own registration"
    );
    let removals = rig.github.count(REMOVAL);
    rig.github.recover(REMOVAL);
    until("the next removal, answered 404", || {
        rig.github.count(REMOVAL) > removals
    })
    .await;
    until("the start at the console", || heard(&rig, &runner).0).await;
    rig.parts.vms.end(job_name(&runner), End::Ok);
    for _ in 0..3 {
        rig.later(RESULT_WAIT + Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    until("no record", || rig.parts.store.load().is_empty()).await;
    until("the start and the end at the console", || {
        heard(&rig, &runner) == (true, true)
    })
    .await;
}

/// Sol, machines.rs:960: the removal of a surplus Actions runner fails at GitHub, which keeps the registration. A
/// job comes, the Actions runner takes it, and its start and end go with the session; the job ends and the Actions
/// runner removes its own registration. A removal the Runner meant to make is not GitHub's word that no job ran:
/// the console hears the job's start and its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_surplus_whose_removal_failed_and_that_then_ran_a_job_is_heard() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let job = rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    rig.github.cancel_job(job);
    rig.github
        .fail_until_recovered(REMOVAL, Reply::status_json(500, &json!({"message": "try again"})));
    rig.later(cf_daemon::IDLE_GRACE + Duration::from_secs(1));
    until("the removal to fail", || {
        rig.github.count(REMOVAL) >= PAST_RETRIES
    })
    .await;
    let next = rig.queue().await;
    until("the Actions runner to be needed again", || {
        rig.github.job_state(next) == Some(JobState::Acquired(rig.github.scale_sets()[0].id))
    })
    .await;
    let set = rig.github.scale_sets()[0].id;
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    assert_eq!(
        rig.github.job_state(next),
        Some(JobState::Running(runner.clone()))
    );
    rig.github.recover(REMOVAL);
    rig.github.complete_job(&runner, "succeeded");
    rig.parts.vms.end(job_name(&runner), End::Ok);
    until("the machine to be gone", || rig.standing().is_empty()).await;
    for _ in 0..3 {
        rig.later(RESULT_WAIT + Duration::from_secs(1));
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
    until("no record", || rig.parts.store.load().is_empty()).await;
    until("the start and the end at the console", || {
        heard(&rig, &runner) == (true, true)
    })
    .await;
}

/// Sol 1: a Runner from before lost a job's start, reached the idle limit, wrote the machine as stopping for no job
/// with its registration still there, and died before the machine went. That stop was a belief, not GitHub's word:
/// the Runner that starts next asks GitHub first, which refuses for the job that runs there, and the machine stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_for_no_job_left_by_a_runner_before_is_asked_of_github_first() {
    let mut rig = Rig::new(1).await;
    rig.service.discover().await;
    rig.queue().await;
    until("a machine for the job", || rig.standing().len() == 1).await;
    let runner = rig.standing()[0].clone();
    let set = rig.github.scale_sets()[0].id;
    no_session_for_a_while(&rig, set);
    rig.github.lose_session(set);
    rig.github.start_job(&runner);
    rig.restart_after(|rig| {
        for mut record in rig.parts.store.load() {
            if let Some(slot) = record.slot.as_mut() {
                let mut saved = slot.saved();
                saved.phase = cf_lifecycle::Phase::Stopping;
                saved.end = Some(cf_lifecycle::End::NoJob);
                *slot = saved.into();
            }
            record.unregistered = false;
            record.rules = 0;
            rig.parts.store.save(&record);
        }
    })
    .await;
    until("GitHub's refusal heard as the job's start", || {
        heard(&rig, &runner).0
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !rig.parts
            .vms
            .stopped()
            .iter()
            .any(|(job, ..)| *job == job_name(&runner)),
        "stops: {:?}",
        rig.parts.vms.stopped()
    );
    assert_eq!(
        rig.standing(),
        vec![runner.clone()],
        "the machine of a running job stands"
    );
}

/// The version the rig's Runner runs.
const RUNNING: &str = "0.1.0+446125c0a1b2";
const NEWER: &str = "0.1.0+5c0ffee0a1b2";

/// The newest status the console has.
fn last_status(console: &FakeConsole) -> Value {
    console.reports().last().expect("a report")["status"].clone()
}

/// What the console heard of one command, each time it heard of it.
fn results_of(console: &FakeConsole, id: &str) -> Vec<Value> {
    results(console)
        .into_iter()
        .filter(|r| r["id"] == json!(id))
        .collect()
}

/// A restart is said and kept before the Runner ends; the one that starts next says it and does not restart again,
/// though the console sends the command until it hears of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_is_said_by_the_runner_that_starts_next_and_not_done_again() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.stop_pump().await;
    let restart = rig.console.queue_command("restart", None);
    rig.service.report().await;
    assert!(rig.service.restart_requested(), "the Runner is to end");
    // It ends before any report carries the result.
    rig.restart().await;
    assert!(!rig.service.restart_requested());
    until("the result at the console", || {
        !results_of(&rig.console, &restart).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let heard = results_of(&rig.console, &restart);
    assert_eq!(heard.len(), 1, "once: {heard:?}");
    assert_eq!(heard[0]["ok"], json!(true));
    assert_eq!(
        heard[0]["message"],
        json!(format!("{RUNNING} ends and its service manager starts it again"))
    );
    assert!(!rig.service.restart_requested(), "the command is not done again");
}

/// The whole update: built while a job runs, no new job while the machine stands, installed once it is gone, and
/// the Runner ends. The Runner that starts next takes the update up: the console's command is not done again, and
/// the result is said once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_waits_for_the_machine_that_stands_installs_and_is_said_once_by_the_next_runner() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.updates.build_gives(Ok(NEWER));
    let update = rig.console.queue_command("update", None);
    rig.settles("no new job while the machine stands", || {
        last_status(&rig.console)["reason"]
            == json!(format!("updating to {NEWER}: waiting for 1 machine to end"))
    })
    .await;
    assert_eq!(last_status(&rig.console)["open"], json!(false));
    rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    rig.github.github_looks();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.standing(), [runner.as_str()], "nothing new starts");
    assert_eq!(
        rig.updates.installed(),
        [],
        "nothing is installed while a machine stands"
    );
    assert!(!rig.service.restart_requested());

    rig.finish(&runner, "succeeded");
    rig.settles("installed", || rig.updates.installed().len() == 1)
        .await;
    let pending = rig.updates.installed()[0].clone();
    assert_eq!(
        pending,
        cf_daemon::update::Pending {
            id: update.clone(),
            from: RUNNING.into(),
            to: NEWER.into(),
            starts: 0,
            failed: None,
        }
    );
    rig.settles("the Runner is to end", || rig.service.restart_requested())
        .await;
    assert_eq!(
        results_of(&rig.console, &update),
        Vec::<Value>::new(),
        "said by the next Runner"
    );

    rig.restart_into(&pending).await;
    // The console sends the command with every answer until its result comes: once a second report of the new Runner
    // came, it went through the answer to the first, and a build that started would be counted within a moment.
    let calls = rig.console.calls();
    until("two reports of the new Runner", || {
        rig.console.calls() >= calls + 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.updates.built(),
        1,
        "the command that comes again is not done again"
    );
    // A Runner that dies after it said it says it again when it starts once more.
    rig.service.said(&update, true, &format!("now runs {NEWER}"));
    rig.service.said(&update, true, &format!("now runs {NEWER}"));
    until("the result at the console", || {
        !results_of(&rig.console, &update).is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let heard = results_of(&rig.console, &update);
    assert_eq!(heard.len(), 1, "once: {heard:?}");
    assert_eq!(heard[0]["ok"], json!(true));
    assert_eq!(rig.updates.built(), 1);
}

/// A slot promised to GitHub before the update held new jobs back may still get its job: the update waits for it,
/// and installs once it is given back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_waits_for_a_slot_promised_before_it_and_installs_once_it_is_given_back() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let promised = rig.machines.reserve(cf_budget::Size::G2).expect("a slot");
    rig.updates.build_gives(Ok(NEWER));
    rig.console.queue_command("update", None);
    until("the update waits for the promised slot", || {
        last_status(&rig.console)["reason"]
            == json!(format!(
                "updating to {NEWER}: waiting for 1 job that may still come"
            ))
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        rig.updates.installed(),
        [],
        "nothing is installed while a job may still come"
    );
    rig.machines.release(promised);
    until("installed", || rig.updates.installed().len() == 1).await;
}

/// Main has nothing newer: said, nothing installed, and the Mac never stopped taking jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_with_nothing_newer_installs_nothing_and_holds_no_job() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.updates.build_gives(Ok(RUNNING));
    let update = rig.console.queue_command("update", None);
    until("the result", || !results_of(&rig.console, &update).is_empty()).await;
    let heard = &results_of(&rig.console, &update)[0];
    assert_eq!(heard["ok"], json!(true));
    assert_eq!(
        heard["message"],
        json!(format!("{RUNNING} runs already: there is nothing newer"))
    );
    assert_eq!(rig.updates.installed(), []);
    assert!(!rig.service.restart_requested());
    rig.job_running().await;
    assert_eq!(rig.standing().len(), 2, "a second job starts beside {runner}");
}

/// A report carries at most `MAX_RESULTS` results. An update whose result comes while that many wait for the
/// console is said all the same, once, and is not done again though the console sends its command meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_whose_result_comes_while_the_most_results_wait_is_said_once_and_done_once() {
    said_once_and_done_once_with_the_most_results_waiting(0, MAX_RESULTS).await;
}

/// An update the console sends after more commands than a report carries results is done once too, and every
/// command is said once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_that_comes_after_more_commands_than_a_report_carries_results_is_said_once_and_done_once() {
    said_once_and_done_once_with_the_most_results_waiting(MAX_RESULTS + 10, MAX_RESULTS + 10).await;
}

/// The console sends `others` commands this Runner answers at once, and an update among them at `at`, all with one
/// answer. The update's build ends only once the answer is done, so that its result comes after theirs; then the
/// Runner reports until the console has heard of everything.
async fn said_once_and_done_once_with_the_most_results_waiting(at: usize, others: usize) {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.stop_pump().await;
    rig.updates.hold_builds();
    rig.updates.build_gives(Err("the source does not build"));
    let mut ids: Vec<String> = (0..at)
        .map(|_| rig.console.queue_command("dance", None))
        .collect();
    let update = rig.console.queue_command("update", None);
    ids.extend((at..others).map(|_| rig.console.queue_command("dance", None)));
    ids.push(update.clone());
    let mut reports = 0;
    while ids.iter().any(|id| results_of(&rig.console, id).is_empty()) {
        assert!(
            reports < 20,
            "the console heard of every command within 20 reports"
        );
        rig.service.report().await;
        // Memory stays bounded: no command is taken while a report's worth of results wait, and only the update's
        // comes over that.
        let kept: Vec<Value> =
            serde_json::from_slice(&std::fs::read(rig.folder.path().join("results.json")).expect("kept"))
                .expect("results");
        assert!(kept.len() <= MAX_RESULTS + 1, "{} results wait", kept.len());
        rig.updates.release_builds();
        reports += 1;
        // An update under way says what came of it before the next report reads the results.
        until("no update under way", || {
            rig.service.status().status.reason.is_none()
        })
        .await;
    }
    // The console sends a command again until it has its result: two more answers show none is done again.
    rig.service.report().await;
    rig.service.report().await;
    assert_eq!(rig.updates.built(), 1, "the update is done once");
    for id in &ids {
        let heard = results_of(&rig.console, id);
        assert_eq!(heard.len(), 1, "{id} is said once: {heard:?}");
    }
    let heard = &results_of(&rig.console, &update)[0];
    assert_eq!(heard["ok"], json!(false));
    assert_eq!(heard["message"], json!("the source does not build"));
    for report in rig.console.reports() {
        let carried = report["results"].as_array().map_or(0, Vec::len);
        assert!(carried <= MAX_RESULTS, "a report carries {carried} results");
    }
}

/// A build that fails is said with its words, shows in the report's update until one works, and changes nothing
/// else: the Runner goes on taking jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_build_that_fails_is_said_and_counted_and_the_mac_goes_on() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    for (round, failures) in [(1, 1), (2, 2)] {
        rig.updates
            .build_gives(Err("error[E0425]: cannot find value `x`"));
        let update = rig.console.queue_command("update", None);
        until("the result", || !results_of(&rig.console, &update).is_empty()).await;
        let heard = &results_of(&rig.console, &update)[0];
        assert_eq!(heard["ok"], json!(false), "round {round}");
        assert_eq!(heard["message"], json!("error[E0425]: cannot find value `x`"));
        until("the failure in the report", || {
            last_status(&rig.console)["update"]["failures"] == json!(failures)
        })
        .await;
    }
    let said = last_status(&rig.console)["update"].clone();
    assert_eq!(said["source"], json!("/src/runner main"));
    assert_eq!(said["error"], json!("error[E0425]: cannot find value `x`"));
    assert!(
        said["checkedAt"].is_string() && said["errorAt"].is_string(),
        "{said}"
    );
    assert!(!rig.service.restart_requested());
    rig.job_running().await;

    rig.updates.build_gives(Ok(RUNNING));
    let update = rig.console.queue_command("update", None);
    until("the result", || !results_of(&rig.console, &update).is_empty()).await;
    until("no failure once one works", || {
        let said = last_status(&rig.console)["update"].clone();
        said["failures"] == json!(0) && said.get("error").is_none()
    })
    .await;
}

/// `update-to` goes the way `update` goes, to the commit it names: the build is asked for that commit, what it built
/// is installed once no machine stands, and `update` alone still asks for the head of the branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_to_a_commit_builds_that_commit_and_installs_what_it_built() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.updates.build_gives(Ok(RUNNING));
    let update = rig.console.queue_command("update", None);
    until("the result", || !results_of(&rig.console, &update).is_empty()).await;
    // The commit that runs, asked for: nothing to do, said as such, and the page says which commit was built.
    rig.updates.hold_builds();
    rig.updates.build_gives(Ok(RUNNING));
    let same = rig
        .console
        .queue_command("update-to", Some(json!({"commit": "446125c0a1b2"})));
    until("building the commit", || {
        last_status(&rig.console)["reason"] == json!("updating to 446125c0a1b2: building")
    })
    .await;
    rig.updates.release_builds();
    until("the result", || !results_of(&rig.console, &same).is_empty()).await;
    assert_eq!(
        (
            &results_of(&rig.console, &same)[0]["ok"],
            &results_of(&rig.console, &same)[0]["message"]
        ),
        (
            &json!(true),
            &json!(format!("{RUNNING} runs already: it is the commit asked for"))
        )
    );
    let runner = rig.job_running().await;
    rig.updates.build_gives(Ok(NEWER));
    let to = rig
        .console
        .queue_command("update-to", Some(json!({"commit": "5c0ffee0a1b2"})));
    rig.settles("no new job while the machine stands", || {
        last_status(&rig.console)["reason"]
            == json!(format!("updating to {NEWER}: waiting for 1 machine to end"))
    })
    .await;
    assert_eq!(rig.updates.installed(), []);
    rig.finish(&runner, "succeeded");
    rig.settles("installed", || rig.updates.installed().len() == 1)
        .await;
    assert_eq!(
        rig.updates.asked(),
        [
            None,
            Some("446125c0a1b2".to_string()),
            Some("5c0ffee0a1b2".to_string())
        ]
    );
    assert_eq!(
        rig.updates.installed(),
        [cf_daemon::update::Pending {
            id: to,
            from: RUNNING.into(),
            to: NEWER.into(),
            starts: 0,
            failed: None,
        }]
    );
    rig.settles("the Runner is to end", || rig.service.restart_requested())
        .await;
}

/// An `update-to` whose arguments name no commit is answered with why, and nothing is built: the Mac goes on taking
/// jobs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_to_that_names_no_commit_says_why_and_builds_nothing() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let not_a_name = "the commit is no commit's name: 7 to 40 lowercase hex characters";
    for (args, why) in [
        (None, "the command names no commit"),
        (Some(json!("5c0ffee")), "the command names no commit"),
        (Some(json!({})), "the command names no commit"),
        (Some(json!({"commit": 5})), not_a_name),
        (Some(json!({"commit": "5C0FFEE"})), not_a_name),
        (Some(json!({"commit": "5c0ffe"})), not_a_name),
        (Some(json!({"commit": "a".repeat(41)})), not_a_name),
        (Some(json!({"commit": "main"})), not_a_name),
        (Some(json!({"commit": "--upload-pack=touch x"})), not_a_name),
        (
            Some(json!({"commit": "5c0ffee", "branch": "pr"})),
            "`branch` is no argument of update-to",
        ),
    ] {
        let id = rig.console.queue_command("update-to", args.clone());
        until("the result", || !results_of(&rig.console, &id).is_empty()).await;
        let heard = &results_of(&rig.console, &id)[0];
        assert_eq!(
            (&heard["ok"], &heard["message"]),
            (&json!(false), &json!(why)),
            "{args:?}"
        );
    }
    assert_eq!(rig.updates.built(), 0);
    assert!(!rig.service.restart_requested());
    rig.job_running().await;
}

/// A Runner with nowhere to update from offers neither update, and says why when one comes all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_with_nowhere_to_update_from_offers_no_update() {
    let mut rig = Rig::new(2).await;
    let alone = service(
        &rig.github,
        &rig.console,
        &rig.parts,
        &rig.machines,
        &rig.token,
        &rig.battery,
        (
            &rig.outbox,
            None,
            &rig.settings,
            rig.folder.path(),
            rig.update_limit,
        ),
    );
    let report = serde_json::to_value(alone.status()).expect("a report");
    assert_eq!(
        report["status"]["commands"],
        json!([
            "pause",
            "resume",
            "restart",
            "stop-runner",
            "settings",
            "test-run"
        ])
    );
    rig.stop_pump().await;
    let asked = [
        rig.console.queue_command("update", None),
        rig.console
            .queue_command("update-to", Some(json!({"commit": "5c0ffee"}))),
        rig.console
            .queue_command("update-to", Some(json!({"commit": "main"}))),
    ];
    alone.report().await;
    alone.report().await;
    for id in &asked {
        let heard = &results_of(&rig.console, id)[0];
        assert_eq!(
            (&heard["ok"], &heard["message"]),
            (&json!(false), &json!("this Runner has nowhere to update from")),
            "{id}"
        );
    }
}

/// Machines that do not end within the update's limit: nothing is installed, and the Mac takes jobs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_whose_machine_outlasts_its_limit_installs_nothing_and_takes_jobs_again() {
    let rig = Rig::limited(2, Duration::from_secs(4)).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.updates.build_gives(Ok(NEWER));
    let update = rig.console.queue_command("update", None);
    until("the hold", || last_status(&rig.console)["open"] == json!(false)).await;
    until("the result", || !results_of(&rig.console, &update).is_empty()).await;
    let heard = &results_of(&rig.console, &update)[0];
    assert_eq!(heard["ok"], json!(false));
    assert_eq!(
        heard["message"],
        json!(format!(
            "{NEWER} was not installed: still waiting for 1 machine to end after 4 seconds"
        ))
    );
    assert_eq!(rig.updates.installed(), []);
    assert!(!rig.service.restart_requested());
    rig.job_running().await;
    assert_eq!(rig.standing().len(), 2, "jobs start again beside {runner}");
}

/// An install that fails is said, and the Mac takes jobs again with the program it has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_install_that_fails_is_said_and_the_mac_takes_jobs_again() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.updates.build_gives(Ok(NEWER));
    rig.updates.install_gives(Err("2 machines have a record"));
    let update = rig.console.queue_command("update", None);
    until("the result", || !results_of(&rig.console, &update).is_empty()).await;
    let heard = &results_of(&rig.console, &update)[0];
    assert_eq!(heard["ok"], json!(false));
    assert_eq!(heard["message"], json!("2 machines have a record"));
    assert!(!rig.service.restart_requested());
    rig.job_running().await;
}

/// While an update builds: the console's command that comes again is not done twice, another update and a restart
/// are refused, and the console sees what it does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn while_an_update_builds_it_runs_once_and_another_update_or_a_restart_is_refused() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.updates.build_takes(Duration::from_millis(1500));
    rig.updates.build_gives(Ok(RUNNING));
    let update = rig.console.queue_command("update", None);
    until("building", || {
        last_status(&rig.console)["reason"] == json!("updating: building")
    })
    .await;
    let second = rig.console.queue_command("update", None);
    let to = rig
        .console
        .queue_command("update-to", Some(json!({"commit": "5c0ffee0a1b2"})));
    let restart = rig.console.queue_command("restart", None);
    until("all refused", || {
        [&second, &to, &restart]
            .iter()
            .all(|id| !results_of(&rig.console, id).is_empty())
    })
    .await;
    for id in [&second, &to] {
        assert_eq!(results_of(&rig.console, id)[0]["ok"], json!(false));
        assert_eq!(
            results_of(&rig.console, id)[0]["message"],
            json!("an update is under way already")
        );
    }
    assert_eq!(results_of(&rig.console, &restart)[0]["ok"], json!(false));
    assert!(!rig.service.restart_requested());
    until("the first is over", || {
        !results_of(&rig.console, &update).is_empty()
    })
    .await;
    assert_eq!(
        rig.updates.built(),
        1,
        "built once though the console sent it with every answer"
    );
}

/// A result said while a report that fails carries the ones before is kept with them: a Runner that dies then
/// still says every one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_result_said_while_a_report_is_under_way_keeps_the_ones_it_carries() {
    let mut rig = Rig::new(2).await;
    rig.stop_pump().await;
    rig.service.said("first", true, "one");
    rig.console.down(true);
    {
        let report = rig.service.report();
        tokio::pin!(report);
        // Polled once: the report is on its way with the first result.
        std::future::poll_fn(|cx| {
            let _ = report.as_mut().poll(cx);
            std::task::Poll::Ready(())
        })
        .await;
        rig.service.said("second", true, "two");
        report.await;
    }
    rig.restart().await;
    rig.console.down(false);
    rig.stop_pump().await;
    rig.service.report().await;
    let ids: BTreeSet<String> = results(&rig.console)
        .iter()
        .map(|result| result["id"].as_str().expect("an id").to_string())
        .collect();
    assert_eq!(ids, BTreeSet::from(["first".into(), "second".into()]));
}

/// The last report of a Runner that ends says so, and not that the Mac serves nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_last_report_of_a_runner_that_ends_says_it_ends() {
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the scale set", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    rig.stop_pump().await;
    rig.service.report().await;
    assert!(last_status(&rig.console).get("notServing").is_none());
    rig.service.shutdown().await;
    let last = last_status(&rig.console);
    assert_eq!(
        last["notServing"],
        json!("the Runner ends; the one that starts next serves them again")
    );
}

/// Until its first look at GitHub ends, a Runner says it starts, not that it serves nothing; a look that could
/// not read GitHub ends it too, and says why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_says_it_starts_until_its_first_look_at_github_ends() {
    for github_answers in [true, false] {
        let mut rig = Rig::new(2).await;
        if !github_answers {
            rig.github.fail(
                "GET /app/installations",
                Reply::status_json(503, &json!({"message": "unavailable"})),
                usize::MAX,
            );
        }
        rig.stop_pump().await;
        rig.service.report().await;
        assert_eq!(last_status(&rig.console)["starting"], json!(true));
        rig.service.discover().await;
        rig.service.report().await;
        let last = last_status(&rig.console);
        assert!(last.get("starting").is_none(), "{last}");
        assert_eq!(last["discoveryError"].is_string(), !github_answers, "{last}");
    }
}

fn changed(id: &str) -> SettingsLook {
    SettingsLook::Changed { id: id.into() }
}

/// New settings: no new job while a machine stands, the page says why, and once it ended the Runner ends to start
/// again on them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_settings_wait_for_the_machine_that_stands_then_the_runner_starts_again() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.settings.says(changed("v2"));
    until("no new job while the machine stands", || {
        last_status(&rig.console)["reason"] == json!("applying new settings: waiting for 1 machine to end")
    })
    .await;
    assert_eq!(last_status(&rig.console)["open"], json!(false));
    rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    rig.github.github_looks();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.standing(), [runner.as_str()], "nothing new starts");
    assert!(!rig.service.restart_requested());

    rig.finish(&runner, "succeeded");
    until("the Runner is to end", || rig.service.restart_requested()).await;
    assert!(rig.standing().is_empty());
}

/// Settings no Runner could start on change nothing: jobs go on, the Runner goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_no_runner_could_start_on_change_nothing() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.settings.says(SettingsLook::Unusable {
        why: "image 2026.10.20 is not installed".into(),
    });
    let looked = rig.settings.looks();
    until("looked at again", || rig.settings.looks() > looked + 3).await;
    rig.job_running().await;
    rig.job_running().await;
    assert_eq!(rig.standing().len(), 2, "jobs start as before");
    assert_eq!(last_status(&rig.console)["open"], json!(true));
    assert!(!rig.service.restart_requested());
    // The console's page says so until the file changes.
    until("the warning at the console", || {
        last_status(&rig.console)["settingsError"]
            == json!("the settings file changed and is not used: image 2026.10.20 is not installed")
    })
    .await;
    assert!(last_status(&rig.console)["settingsErrorAt"].is_string());
    rig.settings.says(SettingsLook::Same);
    until("the warning gone", || {
        last_status(&rig.console).get("settingsError").is_none()
    })
    .await;
}

/// A wait that runs out gives up on that version of the file and takes jobs again; it is not waited for again, and
/// a newer version is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_settings_whose_wait_runs_out_take_jobs_again_until_the_file_changes_again() {
    let rig = Rig::limited(2, Duration::from_secs(4)).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.settings.says(changed("v2"));
    until("the hold", || last_status(&rig.console)["open"] == json!(false)).await;
    // The test's limit is 4 seconds.
    until("the hold lifted", || {
        last_status(&rig.console)["open"] == json!(true)
    })
    .await;
    assert!(!rig.service.restart_requested());
    rig.job_running().await;
    assert_eq!(rig.standing().len(), 2, "jobs start again beside {runner}");
    let looked = rig.settings.looks();
    until("looked at again", || rig.settings.looks() > looked + 3).await;
    assert_eq!(
        last_status(&rig.console)["open"],
        json!(true),
        "not waited for again"
    );
    assert_eq!(
        last_status(&rig.console)["settingsError"],
        json!("new settings were not taken up: still waiting for 1 machine to end after 4 seconds")
    );

    rig.settings.says(changed("v3"));
    until("a newer version is waited for", || {
        last_status(&rig.console)["reason"] == json!("applying new settings: waiting for 2 machines to end")
    })
    .await;
}

/// A file put back as it was while the Runner waited: the hold is lifted and the Runner goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn settings_put_back_during_the_wait_lift_the_hold() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.settings.says(changed("v2"));
    until("the hold", || last_status(&rig.console)["open"] == json!(false)).await;
    // The console's restart would start the next Runner on the new settings beside the machine: refused.
    let restart = rig.console.queue_command("restart", None);
    until("the restart's result", || {
        !results_of(&rig.console, &restart).is_empty()
    })
    .await;
    assert_eq!(results_of(&rig.console, &restart)[0]["ok"], json!(false));
    assert!(!rig.service.restart_requested());
    rig.settings.says(SettingsLook::Same);
    rig.finish(&runner, "succeeded");
    until("the hold lifted", || {
        last_status(&rig.console)["open"] == json!(true)
    })
    .await;
    assert!(!rig.service.restart_requested());
    rig.job_running().await;
}

/// While new settings wait an update is refused, and while an update is under way new settings wait for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn new_settings_and_an_update_never_wait_at_once() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.settings.says(changed("v2"));
    until("the hold", || last_status(&rig.console)["open"] == json!(false)).await;
    let update = rig.console.queue_command("update", None);
    until("the update's result", || {
        !results_of(&rig.console, &update).is_empty()
    })
    .await;
    assert_eq!(results_of(&rig.console, &update)[0]["ok"], json!(false));
    assert_eq!(rig.updates.built(), 0);
    rig.settings.says(SettingsLook::Same);
    rig.finish(&runner, "succeeded");
    until("the hold lifted", || {
        last_status(&rig.console)["open"] == json!(true)
    })
    .await;

    rig.updates.build_takes(Duration::from_millis(1500));
    rig.updates.build_gives(Ok(RUNNING));
    let update = rig.console.queue_command("update", None);
    until("building", || {
        last_status(&rig.console)["reason"] == json!("updating: building")
    })
    .await;
    rig.settings.says(changed("v3"));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        last_status(&rig.console)["reason"],
        json!("updating: building"),
        "the update's, not the settings'"
    );
    assert!(!rig.service.restart_requested());
    // The update finds nothing newer; then the settings are taken up.
    until("then the settings are taken up", || {
        rig.service.restart_requested()
    })
    .await;
    assert_eq!(rig.updates.built(), 1);
    assert_eq!(rig.updates.installed(), []);
    // Its result: at the console, or kept for the next Runner to say.
    let kept: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(rig.folder.path().join("results.json")).unwrap_or_default(),
    )
    .unwrap_or_default();
    let said: Vec<Value> = results_of(&rig.console, &update)
        .into_iter()
        .chain(kept.into_iter().filter(|result| result["id"] == json!(update)))
        .collect();
    assert!(!said.is_empty());
    for result in &said {
        assert_eq!(result["ok"], json!(true));
        assert_eq!(
            result["message"],
            json!(format!("{RUNNING} runs already: there is nothing newer"))
        );
    }
}

/// An update's limit runs from the console's command, the build included: a build that takes most of it leaves the
/// wait for the machines what is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_updates_limit_counts_its_build() {
    let rig = Rig::limited(2, Duration::from_secs(4)).await;
    rig.service.discover().await;
    rig.job_running().await;
    rig.updates.build_takes(Duration::from_millis(3000));
    rig.updates.build_gives(Ok(NEWER));
    let began = std::time::Instant::now();
    let update = rig.console.queue_command("update", None);
    until("the result", || !results_of(&rig.console, &update).is_empty()).await;
    // The test's limit is 4 seconds: with the build's 3 left out, the wait alone would take 4 more.
    assert!(
        began.elapsed() < Duration::from_millis(6000),
        "{:?}",
        began.elapsed()
    );
    assert_eq!(results_of(&rig.console, &update)[0]["ok"], json!(false));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workstation_reports_the_memory_for_jobs_of_its_person_away_or_at_it() {
    let mut settings = settings_for(2);
    settings.mode = Mode::Workstation;
    settings.idle_after = Millis::secs(120);
    settings.away.cap_bytes = 10 * GIB;
    settings.in_use.cap_bytes = 6 * GIB;
    let mut rig = Rig::with_settings(settings).await;
    rig.stop_pump().await;
    rig.machines.with_host(FakeHost::touch);
    assert_eq!(
        rig.report()["status"]["memoryForJobsBytes"],
        json!(6u64 << 30),
        "the person is there"
    );
    pass(&rig.machines, 120);
    let away = rig.report();
    assert_eq!(
        away["status"]["memoryForJobsBytes"],
        json!(10u64 << 30),
        "the person is away"
    );
    assert_eq!(
        away["status"]["maxJobs"],
        json!(2),
        "kept for older consoles: 10 GB of cf-4g"
    );
    rig.machines.with_host(FakeHost::touch);
    assert_eq!(
        rig.report()["status"]["memoryForJobsBytes"],
        json!(6u64 << 30),
        "back at it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_headless_host_reports_its_one_cap_as_the_memory_for_jobs() {
    let settings = Settings::for_headless(cf_daemon::testing::TEST_MAC, 8);
    let cap = settings.away.cap_bytes;
    assert!(cap > 0 && cap < 64 * GIB, "{cap}");
    let mut rig = Rig::with_settings(settings).await;
    rig.stop_pump().await;
    assert_eq!(rig.report()["status"]["memoryForJobsBytes"], json!(cap));
    rig.machines.with_host(FakeHost::touch);
    assert_eq!(
        rig.report()["status"]["memoryForJobsBytes"],
        json!(cap),
        "nobody sits at a headless host: a touch changes nothing"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_actions_runner_reports_its_machines_size_and_what_the_budget_counts_it_with() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the scale set's session", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    let running = rig.job_running().await;
    rig.queue().await;
    until("a second machine", || rig.standing().len() == 2).await;
    let waits = rig
        .standing()
        .into_iter()
        .find(|r| *r != running)
        .expect("the second");
    until("both machines in a report", || {
        rig.console.reports().last().is_some_and(|report| {
            report["status"]["runners"].as_array().is_some_and(|runners| {
                runners.len() == 2
                    && runners
                        .iter()
                        .any(|r| r["name"] == json!(running) && r["job"].is_object())
            })
        })
    })
    .await;
    let report = rig.console.reports().last().cloned().expect("report");
    assert_eq!(
        report["status"]["memoryForJobsBytes"],
        json!(4u64 << 30),
        "room for two cf-2g"
    );
    let runners = report["status"]["runners"].as_array().expect("runners");
    let of = |name: &str| runners.iter().find(|r| r["name"] == json!(name)).expect("runner");
    assert_eq!(of(&running)["size"], json!("cf-2g"));
    assert_eq!(
        of(&waits)["size"],
        json!("cf-2g"),
        "also while it waits for its job"
    );
    assert!(of(&waits).get("job").is_none());
    // Each machine counts with its size, and together they are all the budget holds: the full cap.
    assert_eq!(of(&running)["countedBytes"], json!(Size::G2.vm_bytes()));
    assert_eq!(of(&waits)["countedBytes"], json!(Size::G2.vm_bytes()));
    let counted: u64 = runners
        .iter()
        .map(|r| r["countedBytes"].as_u64().expect("counted"))
        .sum();
    let budget = rig.machines.snapshot().budget;
    assert_eq!(
        budget.slots.len(),
        2,
        "the budget holds the two machines and nothing else"
    );
    assert_eq!(counted, budget.committed_bytes);
    assert_eq!(json!(counted), report["status"]["memoryForJobsBytes"]);
}

/// The console's `settings` command, as the console sends it: written into the settings file, which the Runner then
/// takes up by starting again once no machine stands.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_command_is_written_to_the_file_and_the_runner_starts_again_on_it() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    let id = rig.console.queue_command(
        "settings",
        Some(json!({"maxJobs": 2, "idleMinutes": null, "minFreeMemoryPercent": 8.0, "maxJobHours": 1.5})),
    );
    until("the command's result", || {
        !results_of(&rig.console, &id).is_empty()
    })
    .await;
    let result = &results_of(&rig.console, &id)[0];
    assert_eq!(result["ok"], json!(true), "{result}");
    assert_eq!(
        result["message"],
        json!(
            "the new settings are in the settings file; the Runner starts again on them once no machine stands"
        )
    );
    assert_eq!(
        rig.settings.changes(),
        vec![SettingsChange {
            max_jobs: Some(Some(2)),
            idle_minutes: Some(None),
            min_free_memory_percent: Some(Some(8)),
            deadline_minutes: Some(Some(90)),
        }]
    );
    // The machine of the running job stands until its job is over; then the Runner starts again.
    until("the hold", || last_status(&rig.console)["open"] == json!(false)).await;
    assert!(!rig.service.restart_requested());
    rig.finish(&runner, "succeeded");
    until("the restart", || rig.service.restart_requested()).await;
}

/// A setting the Runner does not have, a value of the wrong kind, and a value the file refuses: each is said in
/// the command's result, and nothing is written or restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_command_the_runner_cannot_take_says_why_and_changes_nothing() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.settings.refuse_next("max_jobs: between 1 and 32");
    for (args, why) in [
        (
            json!({"jobMemoryGb": 6}),
            "memory per job: each job gets the memory its label names, cf-2g to cf-8g",
        ),
        (json!({"maxJobs": 2.5}), "jobs at once: a whole number"),
        (json!({}), "the command changes no setting"),
        (json!({"maxJobs": 40}), "max_jobs: between 1 and 32"),
    ] {
        let id = rig.console.queue_command("settings", Some(args.clone()));
        until("the command's result", || {
            !results_of(&rig.console, &id).is_empty()
        })
        .await;
        let result = &results_of(&rig.console, &id)[0];
        assert_eq!(result["ok"], json!(false), "{args}");
        assert_eq!(result["message"], json!(why), "{args}");
    }
    assert_eq!(rig.settings.changes(), vec![]);
    assert!(!rig.service.restart_requested());
    assert_eq!(last_status(&rig.console)["open"], json!(true));
}

/// While an update is under way new settings are refused: it starts the Runner again itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_settings_command_waits_for_no_update() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    rig.updates.build_takes(Duration::from_millis(1500));
    rig.updates.build_gives(Ok(RUNNING));
    rig.console.queue_command("update", None);
    until("building", || {
        last_status(&rig.console)["reason"] == json!("updating: building")
    })
    .await;
    let id = rig.console.queue_command("settings", Some(json!({"maxJobs": 1})));
    until("the command's result", || {
        !results_of(&rig.console, &id).is_empty()
    })
    .await;
    assert_eq!(
        results_of(&rig.console, &id)[0]["message"],
        json!("an update is under way; send the settings once it is over")
    );
    assert_eq!(rig.settings.changes(), vec![]);
}

/// The report says which commands the Runner does and the limits the `settings` command changes, as they are.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_report_says_the_commands_the_runner_does_and_its_limits() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("a report", || !rig.console.reports().is_empty()).await;
    let report = rig.console.reports().last().expect("a report").clone();
    assert_eq!(
        report["status"]["commands"],
        json!([
            "pause",
            "resume",
            "restart",
            "stop-runner",
            "update",
            "update-to",
            "settings",
            "test-run"
        ])
    );
    assert_eq!(
        report["limits"],
        json!({
            "maxJobs": null,
            // The 5 GiB floor of a 64 GiB dedicated Mac, 7.8125% of its memory, to two places.
            "minFreeMemoryPercent": 7.81,
            "maxJobHours": 6.0,
        }),
        "a dedicated Mac waits for no person"
    );
}

// ---- label sets of queued jobs ----

/// A label the default branch's workflow does not name: only a queued job's set has it.
const PR_LABEL: &str = "pr-only";

/// A public repository the Runner serves on purpose.
const OPEN: &str = "org/open";

/// A job of a pull request that another Actions runner took at once: it is under way, so it counts as seen, and no
/// machine of this Mac ever has it.
fn under_way_elsewhere(rig: &Rig, repo: &str, labels: &[&str]) -> i64 {
    let id = rig
        .github
        .queue_job_at(repo, labels, "build", "refs/pull/7/merge");
    rig.github.take_job(id);
    id
}

/// Waits until the Runner serves a scale set with this label and GitHub has made it: its listener makes it.
async fn serving(rig: &Rig, label: &str) {
    until(&format!("a scale set with {label}"), || {
        served_with(rig, label) == (true, true)
    })
    .await;
}

/// Whether the Runner serves a scale set with this label, and GitHub has it.
fn served_with(rig: &Rig, label: &str) -> (bool, bool) {
    let label = label.to_string();
    (
        rig.service.sets().iter().any(|s| s.labels.contains(&label)),
        rig.github.scale_sets().iter().any(|s| s.labels.contains(&label)),
    )
}

// added: a pull request that adds a job of new labels gets a scale set of them, and its job a machine, though the
// default branch's workflows name no such set; main's own set stays as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_job_of_labels_no_workflow_on_main_names_gets_a_scale_set_and_runs() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    assert_eq!(rig.service.sets().len(), 1, "main's set");
    rig.github.queue_job_at(
        REPO,
        &["self-hosted", "CF-2g", PR_LABEL],
        "build",
        "refs/pull/7/merge",
    );
    rig.service.look_at_queue().await;
    serving(&rig, PR_LABEL).await;
    let set = rig
        .service
        .sets()
        .into_iter()
        .find(|s| s.labels.contains(&PR_LABEL.to_string()))
        .expect("the queued job's set");
    assert_eq!(
        set.labels,
        ["cf-2g", PR_LABEL, "self-hosted"],
        "as a workflow's set is: lowercased, sorted"
    );
    assert_eq!(set.size, Size::G2);
    assert_eq!(rig.service.sets().len(), 2, "main's set and the job's");
    assert_eq!(rig.service.discovery_error(), None);
    // GitHub hands the job to the new set once its polls offered room, as it looks at its queue.
    rig.settles("a machine for the queued job", || {
        rig.github.github_looks();
        rig.standing().len() == 1
    })
    .await;

    // A run under way whose next job waits: its set is served too, and a discover keeps both.
    let ids = rig.github.queue_run(
        REPO,
        &[
            (&["ubuntu-latest"], "lint"),
            (&["self-hosted", "cf-2g", "after-lint"], "test"),
        ],
        "refs/pull/8/merge",
    );
    rig.github.take_job(ids[0]);
    rig.github.take_job(ids[1]);
    rig.service.discover().await;
    serving(&rig, "after-lint").await;
    serving(&rig, PR_LABEL).await;
    assert_eq!(rig.service.sets().len(), 3);
}

// added: a set only queued jobs asked for is served 30 minutes after a job of it was last seen queued or running,
// then closed and deleted as any set nothing asks for. 29 minutes keep it, 31 do not.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_jobs_set_is_kept_30_minutes_after_its_jobs_were_last_seen() {
    let rig = Rig::new(2).await;
    let job = under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    serving(&rig, PR_LABEL).await;
    // Running for 20 minutes counts as seen all along.
    rig.later(Duration::from_secs(20 * 60));
    rig.service.look_at_queue().await;
    rig.github.end_job(job, "success");
    rig.later(Duration::from_secs(29 * 60));
    rig.service.look_at_queue().await;
    rig.service.discover().await;
    serving(&rig, PR_LABEL).await;
    rig.later(Duration::from_secs(2 * 60));
    rig.service.look_at_queue().await;
    assert_eq!(
        served_with(&rig, PR_LABEL),
        (false, false),
        "31 minutes after: closed and deleted"
    );
    assert_eq!(rig.service.sets().len(), 1, "main's set stays");
    assert_eq!(rig.service.discovery_error(), None);

    // Seen again, it is served again.
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.look_at_queue().await;
    serving(&rig, PR_LABEL).await;
}

// added: a set main's workflows stop naming while its jobs still queue is not dropped from under them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_main_stops_naming_stays_while_its_jobs_wait() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    until("main's set at GitHub", || rig.github.scale_sets().len() == 1).await;
    let job = under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g"]);
    rig.github.set_workflow(
        REPO,
        "ci.yml",
        "on: push\njobs:\n  build:\n    runs-on: [self-hosted, cf-2g, renamed]\n    steps: []\n",
    );
    let before: Vec<String> = rig.github.scale_sets().iter().map(|s| s.name.clone()).collect();
    rig.service.discover().await;
    let now: Vec<String> = rig.github.scale_sets().iter().map(|s| s.name.clone()).collect();
    assert!(now.contains(&before[0]), "the old set stays: {now:?}");
    serving(&rig, "renamed").await;
    rig.later(Duration::from_secs(31 * 60));
    rig.service.discover().await;
    assert!(
        rig.github.scale_sets().iter().any(|s| s.name == before[0]),
        "its job still runs"
    );
    rig.github.end_job(job, "success");
    rig.later(Duration::from_secs(31 * 60));
    rig.service.discover().await;
    let now: Vec<String> = rig.github.scale_sets().iter().map(|s| s.name.clone()).collect();
    assert!(
        !now.contains(&before[0]),
        "gone once nothing asked for it for 30 minutes: {now:?}"
    );
}

// added: at most 8 label sets of queued jobs a repository; the newest are served, the rest are said. Main's sets do
// not count.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_repository_gets_at_most_8_sets_of_queued_jobs_the_newest_and_the_rest_are_said() {
    let rig = Rig::new(2).await;
    let labels: Vec<String> = (1..=9).map(|n| format!("pr-{n}")).collect();
    for label in &labels[..8] {
        under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", label]);
    }
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g"]);
    rig.service.discover().await;
    assert_eq!(rig.service.sets().len(), 9, "8 of queued jobs and main's");
    assert_eq!(rig.service.discovery_error(), None);

    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", &labels[8]]);
    rig.service.look_at_queue().await;
    assert_eq!(rig.service.sets().len(), 9);
    assert_eq!(served_with(&rig, "pr-1"), (false, false), "the oldest goes");
    for label in &labels[1..] {
        serving(&rig, label).await;
    }
    let error = rig.service.discovery_error().expect("said");
    assert!(error.contains(REPO) && error.contains("at most 8"), "{error}");
}

// added: a set the bound pushes out while GitHub is making it goes at GitHub in the same look: its listener ends
// before the sets are listed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_pushed_out_while_github_makes_it_is_deleted_in_the_same_look() {
    let rig = Rig::new(2).await;
    // GitHub is slow to make scale sets: the look that pushes the oldest out comes while it is made.
    rig.github.delay(
        "POST /svc/_apis/runtime/runnerscalesets",
        Duration::from_secs(1),
        9,
    );
    let labels: Vec<String> = (1..=9).map(|n| format!("pr-{n}")).collect();
    for label in &labels[..8] {
        under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", label]);
    }
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g"]);
    rig.service.discover().await;

    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", &labels[8]]);
    rig.service.look_at_queue().await;
    assert_eq!(rig.service.sets().len(), 9);
    assert_eq!(
        served_with(&rig, "pr-1"),
        (false, false),
        "the oldest goes, at GitHub too"
    );
    for label in &labels[1..] {
        serving(&rig, label).await;
    }
}

// added: a set the bound pushes out goes though GitHub has not made it yet: its listener is still trying, and the
// set must not stay served beside the newest.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_pushed_out_before_github_made_it_is_not_served() {
    let rig = Rig::new(2).await;
    // No scale set is made at GitHub until it recovers: every listener is still trying.
    rig.github.fail_until_recovered(
        "POST /svc/_apis/runtime/runnerscalesets",
        Reply::text(503, "unavailable"),
    );
    let labels: Vec<String> = (1..=9).map(|n| format!("pr-{n}")).collect();
    for label in &labels[..8] {
        under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", label]);
    }
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g"]);
    rig.service.discover().await;
    assert_eq!(rig.service.sets().len(), 9, "8 of queued jobs and main's");

    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", &labels[8]]);
    rig.service.look_at_queue().await;
    assert_eq!(
        rig.service.sets().len(),
        9,
        "the oldest goes though GitHub never had it"
    );
    assert!(!served_with(&rig, "pr-1").0, "pr-1 is no longer served");
    rig.github.recover("POST /svc/_apis/runtime/runnerscalesets");
    for label in &labels[1..] {
        serving(&rig, label).await;
    }
    assert_eq!(
        served_with(&rig, "pr-1"),
        (false, false),
        "and GitHub does not get it later"
    );
}

// added: at most 32 label sets of queued jobs on the host, whatever repositories ask.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_host_serves_at_most_32_sets_of_queued_jobs() {
    let rig = Rig::new(2).await;
    let repos = ["org/a", "org/b", "org/c", "org/d", "org/e"];
    for repo in repos {
        rig.github.add_repo(repo, true);
    }
    for (r, repo) in repos.iter().take(4).enumerate() {
        for n in 0..8 {
            under_way_elsewhere(&rig, repo, &["self-hosted", "cf-2g", &format!("set-{r}-{n}")]);
        }
    }
    rig.service.discover().await;
    assert_eq!(rig.service.sets().len(), 33, "32 of queued jobs and main's");
    assert_eq!(rig.service.discovery_error(), None);
    under_way_elsewhere(&rig, "org/e", &["self-hosted", "cf-2g", "set-4-0"]);
    rig.service.look_at_queue().await;
    assert_eq!(rig.service.sets().len(), 33);
    serving(&rig, "set-4-0").await;
    assert_eq!(served_with(&rig, "set-0-0"), (false, false), "the oldest goes");
    let error = rig.service.discovery_error().expect("said");
    assert!(error.contains("at most 32"), "{error}");
}

// added: on a public repository served on purpose, a pull request from a fork is a stranger's code: its new labels
// get no set, while those of a pull request from a branch of the repository do. On a private repository, a fork's
// pull request is code of someone who may read the repository, and its set is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forks_pull_request_gets_no_set_on_a_public_repository() {
    let rig = Rig::new(2).await;
    rig.github.add_repo(OPEN, false);
    let stranger = under_way_elsewhere(&rig, OPEN, &["self-hosted", "cf-2g", "fork-only"]);
    rig.github.from_fork(stranger);
    under_way_elsewhere(&rig, OPEN, &["self-hosted", "cf-2g", "branch-only"]);
    let colleague = under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", "private-fork"]);
    rig.github.from_fork(colleague);
    rig.service.discover().await;
    serving(&rig, "branch-only").await;
    serving(&rig, "private-fork").await;
    assert_eq!(served_with(&rig, "fork-only"), (false, false));
    assert_eq!(
        rig.service.discovery_error(),
        None,
        "a stranger's set needs no word"
    );
}

// added: a repository the Runner does not serve (public, not listed) is not asked about its queue at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_queue_of_a_repository_the_runner_does_not_serve_is_not_read() {
    let rig = Rig::new(2).await;
    rig.github.add_repo("org/public", false);
    under_way_elsewhere(&rig, "org/public", &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    rig.service.look_at_queue().await;
    assert_eq!(rig.github.count("org/public/actions"), 0);
    assert_eq!(served_with(&rig, PR_LABEL), (false, false));
    assert!(
        rig.github.count("org/app/actions/runs") > 0,
        "the served one's is"
    );
}

// added: a queued job's labels are served under a workflow's rules: one of ours, no other platform, only host
// labels this host has, nothing odd. A control set that keeps them is served.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_jobs_this_host_must_not_take_get_no_set() {
    let rig = Rig::new(2).await;
    let long = "x".repeat(300);
    let many: Vec<String> = (0..40).map(|n| format!("l{n}")).collect();
    let mut many: Vec<&str> = many.iter().map(String::as_str).collect();
    many.extend(["self-hosted", "cf-2g"]);
    let seventeen: Vec<String> = (0..15).map(|n| format!("m{n}")).collect();
    let mut seventeen: Vec<&str> = seventeen.iter().map(String::as_str).collect();
    seventeen.extend(["self-hosted", "cf-2g"]);
    for labels in [
        &["self-hosted", "cf-2g", "windows"][..],
        &["self-hosted", "cf-2g", "macos-latest"],
        &["ubuntu-latest"],
        &["gpu-box", "big"],
        &["self-hosted", "cf-2g", "cf-linux"],
        &["self-hosted", "cf-2g", "a,b"],
        &["self-hosted", "cf-2g", "😀"],
        &["self-hosted", "cf-2g", "evil\u{202e}gpj"],
        &["self-hosted", "cf-2g", "line\nbreak"],
        &["self-hosted", "cf-2g", &long],
        &many,
        &seventeen,
        &["self-hosted", "cf-2g", "cf-docker"],
    ] {
        under_way_elsewhere(&rig, REPO, labels);
    }
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", "cf-macos", "fine"]);
    rig.service.discover().await;
    rig.service.look_at_queue().await;
    let sets: Vec<Vec<String>> = rig.service.sets().into_iter().map(|s| s.labels).collect();
    assert_eq!(sets.len(), 2, "main's and the control's only: {sets:?}");
    serving(&rig, "fine").await;
    // Main's set reaches GitHub on its own listener, not necessarily before the control's.
    until("both sets at GitHub", || rig.github.scale_sets().len() == 2).await;
    assert_eq!(rig.github.scale_sets().len(), 2);
    assert_eq!(
        rig.service.discovery_error(),
        None,
        "a job's odd labels are no problem of the host"
    );
}

// added: without the App's "Actions: read" GitHub refuses the queue: main's sets are served as before and the
// console hears what is missing. A refusal or a failed read drops no set a queued job had.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queue_that_cannot_be_read_serves_mains_sets_keeps_the_others_and_says_why() {
    let rig = Rig::new(2).await;
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    serving(&rig, PR_LABEL).await;

    let denied = json!({"message": "Resource not accessible by integration"});
    rig.github
        .fail("/actions/runs", Reply::status_json(403, &denied), 1000);
    rig.later(Duration::from_secs(31 * 60));
    rig.service.discover().await;
    rig.service.look_at_queue().await;
    let error = rig.service.discovery_error().expect("said");
    assert!(error.contains("Actions: read") && error.contains(REPO), "{error}");
    serving(&rig, PR_LABEL).await;
    assert_eq!(rig.service.sets().len(), 2);
    let (_, problems) = rig.service.survey().await;
    assert!(
        problems.iter().any(|p| p.contains("Actions: read")),
        "{problems:?}"
    );

    // GitHub away: the same.
    let rig = Rig::new(2).await;
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    rig.github
        .fail("/actions/runs", Reply::text(502, "bad gateway"), 1000);
    rig.later(Duration::from_secs(31 * 60));
    rig.service.look_at_queue().await;
    rig.service.discover().await;
    serving(&rig, PR_LABEL).await;
    assert!(
        rig.service
            .discovery_error()
            .is_some_and(|e| e.contains("queued jobs"))
    );
}

// added: a rate limit GitHub answers with 403 is no missing permission: the console is not told to grant one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rate_limited_queue_is_not_said_to_need_actions_read() {
    let rig = Rig::new(2).await;
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    serving(&rig, PR_LABEL).await;

    let limited = json!({"message": "API rate limit exceeded for installation ID 1."});
    rig.github
        .fail("/actions/runs", Reply::status_json(403, &limited), 1000);
    rig.later(Duration::from_secs(31 * 60));
    rig.service.look_at_queue().await;
    let error = rig.service.discovery_error().expect("said");
    assert!(
        error.contains("queued jobs could not be read") && !error.contains("Actions: read"),
        "{error}"
    );
    serving(&rig, PR_LABEL).await;
}

// added: an App without "Actions: read" from the start still serves main's sets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_actions_read_from_the_start_mains_sets_are_served() {
    let rig = Rig::new(2).await;
    let denied = json!({"message": "Resource not accessible by integration"});
    rig.github
        .fail("/actions/runs", Reply::status_json(403, &denied), 1000);
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    assert_eq!(rig.service.sets().len(), 1);
    assert_eq!(served_with(&rig, PR_LABEL), (false, false));
    let error = rig.service.discovery_error().expect("said");
    assert!(error.contains("Actions: read"), "{error}");
    rig.queue().await;
    until("main's job runs", || rig.standing().len() == 1).await;
}

// added: a queue that did not change costs 304s and no read of a run's jobs, and still counts as seeing them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unchanged_queue_is_answered_304_and_its_sets_stay() {
    let rig = Rig::new(2).await;
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-2g", PR_LABEL]);
    rig.service.discover().await;
    let (before, jobs) = (rig.github.not_modified(), rig.github.count("/jobs"));
    for _ in 0..2 {
        rig.later(Duration::from_secs(29 * 60));
        rig.service.look_at_queue().await;
    }
    assert_eq!(rig.github.not_modified(), before + 4, "both listings, twice");
    assert_eq!(
        rig.github.count("/jobs"),
        jobs,
        "the run did not change: its jobs are not asked for"
    );
    serving(&rig, PR_LABEL).await;
}

// added: a look at GitHub that hangs (here the queue's) holds up neither the console's news nor a stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_look_at_github_holds_up_neither_the_news_nor_a_stop() {
    let mut rig = Rig::new(2).await;
    rig.stop_pump().await;
    rig.github.fail(
        "/actions/runs",
        Reply::After(
            Duration::from_secs(60),
            Box::new(Reply::status_json(200, &json!({}))),
        ),
        1000,
    );
    let (stop, stopped) = cf_scaleset::shutdown();
    let service = rig.service.clone();
    let running = tokio::spawn(async move { service.run(stopped).await });
    until("the look at the queue began", || {
        rig.github.count("/actions/runs") > 0
    })
    .await;
    assert!(rig.outbox.push(started("mac-a-j1")));
    until("the start on the console's page while the look hangs", || {
        rig.console.job_event_ids() == ["mac-a-j1/started"]
    })
    .await;
    let asked = std::time::Instant::now();
    stop.trigger();
    tokio::time::timeout(Duration::from_secs(15), running)
        .await
        .expect("the Runner ended")
        .expect("no panic");
    assert!(asked.elapsed() < Duration::from_secs(12), "{:?}", asked.elapsed());
}

// added: a queued job's set of a size the host's settings stop running goes at the next look at the queue, as main's
// would at the next discovery; main's set of a size it still runs stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_jobs_set_of_a_size_the_settings_stop_goes() {
    let rig = Rig::new(2).await;
    under_way_elsewhere(&rig, REPO, &["self-hosted", "cf-4g", PR_LABEL]);
    rig.service.discover().await;
    serving(&rig, PR_LABEL).await;
    rig.machines.set_settings(settings_for(1)).expect("settings");
    rig.service.look_at_queue().await;
    assert_eq!(served_with(&rig, PR_LABEL), (false, false));
    assert_eq!(rig.service.sets().len(), 1, "main's cf-2g set stays");
}

/// A job GitHub holds back from a Mac with room: queued while the Mac had no room, then room came back and GitHub
/// did not look at its queue again. Says its ID.
async fn held_job(rig: &Rig) -> i64 {
    held_jobs(rig, &[&["self-hosted", "cf-2g"]]).await[0]
}

/// Jobs of these labels GitHub holds back from a Mac with room, as [`held_job`]. Says their IDs.
async fn held_jobs(rig: &Rig, labels: &[&[&str]]) -> Vec<i64> {
    rig.service.discover().await;
    rig.settles("the scale set's session", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    hold(rig, labels).await
}

/// Jobs of these labels queued while the Mac had no room, then room came back and GitHub did not look at its queue
/// again. Says their IDs.
async fn hold(rig: &Rig, labels: &[&[&str]]) -> Vec<i64> {
    let short = rig.parts.settings.away.floor_bytes + (1 << 30);
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = short));
    rig.settles("no room offered", || !rig.github.offers_room(REPO))
        .await;
    let jobs: Vec<i64> = labels
        .iter()
        .map(|labels| rig.github.queue_job(REPO, labels, "build"))
        .collect();
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = 64 << 30));
    rig.settles("room offered again", || rig.github.offers_room(REPO))
        .await;
    for &job in &jobs {
        assert_eq!(
            rig.github.job_state(job),
            Some(JobState::Available),
            "GitHub holds it"
        );
    }
    jobs
}

/// The labels of the test's scale set, as GitHub registers them.
fn set_labels() -> Vec<String> {
    vec!["cf-2g".to_string(), "self-hosted".to_string()]
}

/// The Actions runners of their own: registered at GitHub outside any scale set, by name.
fn own_runners(rig: &Rig) -> Vec<String> {
    rig.github
        .plain_runners()
        .into_iter()
        .map(|(name, repo, labels)| {
            assert_eq!((repo.as_str(), labels), (REPO, set_labels()), "{name}");
            name
        })
        .collect()
}

/// The one Actions runner of its own, once its machine is up and waits for a job.
async fn the_own_runner(rig: &Rig) -> String {
    let own = own_runners(rig);
    assert_eq!(own.len(), 1, "one Actions runner of its own: {own:?}");
    let own = own[0].clone();
    rig.settles("its machine up and waiting for a job", || {
        rig.machines
            .snapshot()
            .machines
            .iter()
            .any(|machine| machine.runner == own && machine.state == "idle")
    })
    .await;
    own
}

// added: a job GitHub holds while the Mac has room is not left to wait for GitHub's own look. After two minutes of
// waiting and of room, the Mac starts an Actions runner of its own with the job's labels, says so once, and the
// console hears of the held job; its arrival makes GitHub look, the scale set gets the job within three minutes of
// its queueing, and the Actions runner of its own, which ran nothing, is removed with GitHub's 204.
#[tokio::test]
async fn a_job_github_holds_from_a_mac_with_room_runs_within_three_minutes() {
    let log = support::Log::start();
    let rig = Rig::new(2).await;
    let job = held_job(&rig).await;
    // 100 s: GitHub may still look by itself.
    rig.later(Duration::from_secs(100));
    rig.service.look_at_queue().await;
    assert_eq!(own_runners(&rig), Vec::<String>::new());
    assert_eq!(rig.report()["status"].get("heldJobs"), None);

    rig.later(Duration::from_secs(30));
    rig.service.look_at_queue().await;
    let own = the_own_runner(&rig).await;
    let said = log.lines("an Actions runner of its own takes it");
    assert_eq!(said.len(), 1, "{said:?}");
    for words in [
        format!("runner={own}"),
        format!("repo={REPO}"),
        "labels=cf-2g,self-hosted".to_string(),
        format!("job={job}"),
        "jobs=1".to_string(),
    ] {
        assert!(said[0].contains(&words), "{words} in {}", said[0]);
    }
    let status = rig.report()["status"].clone();
    let held = status["heldJobs"].as_array().expect("heldJobs").clone();
    assert_eq!(held.len(), 1, "{held:?}");
    assert_eq!(held[0]["repo"], json!(REPO));
    assert_eq!(held[0]["labels"], json!(set_labels()));
    assert_eq!(held[0]["jobs"], json!(1));
    let waited = held[0]["oldestWaitSeconds"].as_u64().expect("seconds");
    assert!((130..160).contains(&waited), "{waited}");
    rig.settles("the console took a report with it", || {
        rig.console
            .reports()
            .last()
            .is_some_and(|r| r["status"]["heldJobs"][0]["jobs"] == json!(1))
    })
    .await;

    // It comes online, GitHub looks, and the scale set's Actions runner takes the job.
    rig.github.runner_online(&own);
    rig.github.start_job(&own);
    rig.settles("a machine for the job", || rig.standing().len() == 2)
        .await;
    let runner = rig
        .standing()
        .into_iter()
        .find(|r| *r != own)
        .expect("the scale set's");
    rig.github.start_job(&runner);
    assert_eq!(rig.github.job_state(job), Some(JobState::Running(runner.clone())));

    rig.later(Duration::from_secs(60));
    rig.service.look_at_queue().await;
    rig.settles("the Actions runner of its own removed with a 204", || {
        rig.github.removed_at_request().contains(&own)
    })
    .await;
    rig.settles("its machine gone", || !rig.standing().contains(&own))
        .await;
    assert_eq!(rig.standing(), std::slice::from_ref(&runner));
    assert_eq!(rig.report()["status"].get("heldJobs"), None, "nothing held now");
    rig.finish(&runner, "succeeded");
    rig.settles("the job heard", || heard(&rig, &runner) == (true, true))
        .await;
    assert_eq!(
        heard(&rig, &own),
        (false, false),
        "the Actions runner of its own ran nothing"
    );
    assert_eq!(log.lines("an Actions runner of its own takes it").len(), 1);
}

// added: the Actions runner of its own takes the held job as GitHub looks at its queue at the same moment: GitHub
// hands it out once, the scale set starts no machine for it, the removal GitHub refuses for the job is the job's
// start, and the console hears of the job once, from the machine that ran it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_job_the_actions_runner_of_its_own_takes_runs_once() {
    let rig = Rig::new(2).await;
    let job = held_job(&rig).await;
    rig.later(Duration::from_secs(130));
    rig.service.look_at_queue().await;
    let own = the_own_runner(&rig).await;
    rig.github.start_job(&own);
    rig.github.runner_online(&own);
    rig.github.github_looks();
    assert_eq!(rig.github.job_state(job), Some(JobState::Running(own.clone())));

    rig.later(Duration::from_secs(60));
    rig.service.look_at_queue().await;
    rig.settles("its job heard from the refused removal", || {
        heard(&rig, &own) == (true, false)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(rig.standing(), std::slice::from_ref(&own), "no second machine");
    assert!(!rig.github.removed_at_request().contains(&own));
    assert_eq!(rig.github.job_state(job), Some(JobState::Running(own.clone())));

    rig.finish(&own, "succeeded");
    rig.settles("its end heard", || heard(&rig, &own) == (true, true))
        .await;
    assert_eq!(rig.events().len(), 2, "{:?}", rig.events());
    rig.nothing_left().await;
}

// added: an Actions runner of its own that took the held job waits for none, so a job GitHub holds behind it while the
// Mac has room gets another once five minutes are over, not only once that job ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_held_behind_one_that_took_its_job_gets_another() {
    let rig = Rig::new(3).await;
    let job = held_job(&rig).await;
    rig.later(Duration::from_secs(130));
    rig.service.look_at_queue().await;
    let first = the_own_runner(&rig).await;
    rig.github.start_job(&first);
    assert_eq!(rig.github.job_state(job), Some(JobState::Running(first.clone())));
    rig.later(Duration::from_secs(60));
    rig.service.look_at_queue().await;
    rig.settles("its job heard from the refused removal", || {
        heard(&rig, &first) == (true, false)
    })
    .await;

    // Another job, queued while the Mac had no room, then held while it has room again.
    hold(&rig, &[&["self-hosted", "cf-2g"]]).await;
    rig.later(Duration::from_secs(250));
    rig.service.look_at_queue().await;
    let own = own_runners(&rig);
    assert_eq!(own.len(), 2, "a second beside the one that runs a job: {own:?}");
    assert!(own.contains(&first));
    assert_eq!(rig.github.job_state(job), Some(JobState::Running(first)));
}

// added: an Actions runner of its own carries all the held job's labels, so GitHub could give it a job of the same
// repository that asks for only some of them and wants a machine of another size. While such a job waits, the held
// job gets none; the Mac says why once, and the console still hears of the held job.
#[tokio::test]
async fn a_held_job_gets_none_of_its_own_while_one_with_fewer_labels_waits() {
    let log = support::Log::start();
    let rig = Rig::new(3).await;
    let jobs = held_jobs(&rig, &[&["self-hosted", "cf-2g"], &["self-hosted"]]).await;
    let mut own_of_the_set = BTreeSet::new();
    for _ in 0..4 {
        rig.later(Duration::from_secs(40));
        rig.service.look_at_queue().await;
        own_of_the_set.extend(
            rig.github
                .plain_runners()
                .into_iter()
                .filter(|(_, _, labels)| *labels == set_labels())
                .map(|(name, ..)| name),
        );
    }
    assert_eq!(own_of_the_set, BTreeSet::new());
    assert_eq!(rig.github.job_state(jobs[1]), Some(JobState::Available));
    let said = log.lines("only some of its labels");
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].contains(&format!("job={}", jobs[0])), "{}", said[0]);
    let held = rig.report()["status"]["heldJobs"].clone();
    assert!(
        held.as_array()
            .expect("heldJobs")
            .iter()
            .any(|set| set["labels"] == json!(set_labels()) && set["jobs"] == json!(1)),
        "{held}"
    );
}

// added: a job with fewer labels that already runs elsewhere is no job GitHub could give an Actions runner of the held
// job's own: the held job gets one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_with_fewer_labels_that_runs_already_keeps_none_back() {
    let rig = Rig::new(3).await;
    let running = under_way_elsewhere(&rig, REPO, &["self-hosted"]);
    held_job(&rig).await;
    rig.later(Duration::from_secs(130));
    rig.service.look_at_queue().await;
    assert_eq!(own_runners(&rig).len(), 1);
    assert_eq!(rig.github.job_state(running), Some(JobState::TakenElsewhere));
}

// added: a paused Mac starts no Actions runner of its own for a job that waits, and says no job is held: GitHub holds
// none from a Mac that offers no room. Once the pause ends, room comes back and the held job gets one after two more
// minutes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_mac_starts_nothing_for_a_job_that_waits() {
    let rig = Rig::new(2).await;
    held_job(&rig).await;
    rig.console.pause(true);
    rig.settles("the Mac is paused and offers no room", || {
        rig.console
            .reports()
            .last()
            .is_some_and(|r| r["status"]["paused"] == json!(true))
            && !rig.github.offers_room(REPO)
    })
    .await;
    for _ in 0..3 {
        rig.later(Duration::from_secs(150));
        rig.service.look_at_queue().await;
    }
    assert_eq!(own_runners(&rig), Vec::<String>::new());
    assert_eq!(rig.report()["status"].get("heldJobs"), None);
    assert!(rig.standing().is_empty());

    rig.console.pause(false);
    rig.settles("room offered again", || rig.github.offers_room(REPO))
        .await;
    rig.service.look_at_queue().await;
    assert_eq!(
        own_runners(&rig),
        Vec::<String>::new(),
        "room for less than two minutes"
    );
    rig.later(Duration::from_secs(130));
    rig.service.look_at_queue().await;
    assert_eq!(own_runners(&rig).len(), 1);
}

// added: a full Mac starts no Actions runner of its own for a job that waits for its slot, and says none is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_mac_starts_nothing_for_a_job_that_waits() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    rig.settles("no room offered", || !rig.github.offers_room(REPO))
        .await;
    let job = rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build");
    for _ in 0..3 {
        rig.later(Duration::from_secs(150));
        rig.service.look_at_queue().await;
    }
    assert_eq!(own_runners(&rig), Vec::<String>::new());
    assert_eq!(rig.report()["status"].get("heldJobs"), None);
    assert_eq!(rig.standing(), std::slice::from_ref(&runner));
    assert_eq!(rig.github.job_state(job), Some(JobState::Available));
}

// added: however many jobs GitHub holds and however often the queue is read, a scale set gets one Actions runner of its
// own at a time and one in five minutes at most.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_jobs_get_one_actions_runner_of_their_own_at_a_time() {
    let rig = Rig::new(3).await;
    held_job(&rig).await;
    for _ in 0..9 {
        rig.github.queue_job(REPO, &["self-hosted", "cf-2g"], "more");
    }
    // A job queued is a look: GitHub hands one to the Mac's room. The rest it holds.
    rig.settles("the one GitHub handed over has a machine", || {
        rig.standing().len() == 1
    })
    .await;
    let mut seen = BTreeSet::new();
    for _ in 0..20 {
        rig.later(Duration::from_secs(10));
        rig.service.look_at_queue().await;
        seen.extend(own_runners(&rig));
    }
    assert_eq!(seen.len(), 1, "200 s, many looks: {seen:?}");
    let status = rig.report()["status"].clone();
    assert_eq!(status["heldJobs"][0]["jobs"], json!(9));
    for _ in 0..10 {
        let _ = rig.report();
    }
    assert_eq!(own_runners(&rig).len(), 1, "a report starts nothing");
}

/// The console's `test-run` for a run of this id, on this console, as the console sends it.
fn test_run_args(console: &FakeConsole, run: &str, wait_s: u64) -> Value {
    json!({
        "run": run,
        "url": format!("{}/agent/runners/runs/{run}", console.url()),
        "token": format!("cfrt_{}", "t".repeat(43)),
        "size": "cf-2g",
        "docker": false,
        "timeoutS": 600,
        "waitS": wait_s,
        "repo": "codefusion-cc/codefusion-console",
        "label": "feat/x: npm test"
    })
}

/// A test run from the console's command to its end: one machine however often the console sends the command, its
/// start and end at the console as a job's, and a command it cannot take answered with why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_run_starts_one_machine_and_reports_it_as_a_job() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let run = "tr_abcdefghijklmnopqrstuvwxyz";
    let machine = "tr-abcdefghijklmnopqrstuvwxyz";
    let bad = rig.console.queue_command(
        "test-run",
        Some(json!({"run": run, "url": "https://elsewhere.example/agent/runners/runs/x"})),
    );
    let id = rig
        .console
        .queue_command("test-run", Some(test_run_args(&rig.console, run, 0)));
    rig.settles("both results", || {
        !results_of(&rig.console, &id).is_empty() && !results_of(&rig.console, &bad).is_empty()
    })
    .await;
    assert_eq!(results_of(&rig.console, &id)[0]["ok"], json!(true));
    assert_eq!(
        results_of(&rig.console, &id)[0]["message"],
        json!(format!("started {machine}"))
    );
    assert_eq!(results_of(&rig.console, &bad)[0]["ok"], json!(false));
    rig.settles("its start at the console", || {
        rig.console
            .job_event_ids()
            .contains(&format!("{machine}/started"))
    })
    .await;
    let started: Vec<_> = rig
        .parts
        .vms
        .started()
        .into_iter()
        .filter(|request| request.job == job_name(machine))
        .collect();
    assert_eq!(started.len(), 1, "the command was done twice");
    let event = rig
        .console
        .job_events()
        .into_iter()
        .find(|event| event["id"] == json!(format!("{machine}/started")))
        .expect("the start");
    assert_eq!(event["workflow"], json!("test-remote"));
    assert_eq!(event["event"], json!("test-run"));
    assert_eq!(event["job"], json!("feat/x: npm test"));
    let runners = &rig.report()["status"]["runners"];
    assert!(
        runners
            .as_array()
            .is_some_and(|all| all.iter().any(|r| r["name"] == json!(machine))),
        "{runners}"
    );

    rig.parts.vms.end(job_name(machine), End::Ok);
    rig.settles("its end at the console", || {
        rig.console
            .job_event_ids()
            .contains(&format!("{machine}/completed"))
    })
    .await;
    let end = rig
        .console
        .job_events()
        .into_iter()
        .find(|event| event["id"] == json!(format!("{machine}/completed")))
        .expect("the end");
    assert_eq!(
        (&end["result"], &end["end"]),
        (&json!("success"), &json!("success"))
    );
    assert!(
        rig.github.runners().is_empty(),
        "something was registered at GitHub"
    );
    rig.settles("its slot back", || {
        rig.machines.snapshot().budget.slots.is_empty()
    })
    .await;
}

/// A test run that waits for room never holds up the reports, is not taken again while it waits, and says when its
/// wait ran out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_test_run_that_waits_holds_up_no_report_and_says_when_its_wait_ran_out() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    let run = "tr_bbbbbbbbbbbbbbbbbbbbbbbbbb";
    let id = rig
        .console
        .queue_command("test-run", Some(test_run_args(&rig.console, run, 2)));
    let reports = rig.console.reports().len();
    rig.settles("its result", || !results_of(&rig.console, &id).is_empty())
        .await;
    assert!(
        rig.console.reports().len() >= reports + 2,
        "the reports waited for the run"
    );
    let results = results_of(&rig.console, &id);
    assert_eq!(results.len(), 1, "it was answered twice: {results:?}");
    assert_eq!(results[0]["ok"], json!(false));
    assert_eq!(results[0]["message"], json!("busy: no room within 2s"));
    assert_eq!(rig.machines.snapshot().waiting, 0);
    assert_eq!(rig.standing(), [runner.as_str()]);
}

/// A test run that waits for room and that the console stops sending (it ended or was cancelled there) gives its
/// place back and is answered no more; the room that comes is not its.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_waiting_test_run_the_console_no_longer_sends_gives_its_place_back() {
    let rig = Rig::new(1).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    let id = rig.console.queue_command(
        "test-run",
        Some(test_run_args(&rig.console, "tr_cccccccccccccccccccccccccc", 3600)),
    );
    rig.settles("its place in line", || rig.machines.snapshot().waiting == 1)
        .await;
    rig.console.withdraw_command(&id);
    rig.settles("its place given back", || rig.machines.snapshot().waiting == 0)
        .await;
    rig.finish(&runner, "succeeded");
    rig.settles("the CI job's end", || rig.events().len() == 2).await;
    let reports = rig.console.reports().len();
    rig.settles("a few more reports", || {
        rig.console.reports().len() >= reports + 3
    })
    .await;
    assert!(
        results_of(&rig.console, &id).is_empty(),
        "the withdrawn run was answered"
    );
    assert_eq!(
        rig.parts.vms.started().len(),
        1,
        "a machine started for the withdrawn run"
    );
}

/// A Runner that gives [`REPO`] Docker, beside the rig's own, whose pump is stopped: the test drives it.
async fn docker_runner(rig: &mut Rig) -> TestService {
    rig.stop_pump().await;
    rig.github.set_workflow(
        REPO,
        "docker.yml",
        "on: push\njobs:\n  image:\n    runs-on: [self-hosted, cf-2g, cf-docker]\n    steps: []\n",
    );
    service_with(
        &rig.github,
        &rig.console,
        &rig.parts,
        &rig.machines,
        &rig.token,
        &rig.battery,
        (
            &rig.outbox,
            None,
            &rig.settings,
            rig.folder.path(),
            rig.update_limit,
        ),
        |config| config.policy.docker_repos = vec![REPO.into()],
    )
}

/// Whether the Runner serves a `cf-docker` label set.
fn serves_docker(service: &TestService) -> bool {
    service
        .sets()
        .iter()
        .any(|set| set.labels.iter().any(|label| label == "cf-docker"))
}

/// The test run the console sends with an image, answered by `service`: its result.
async fn docker_run(rig: &Rig, service: &TestService, run: &str) -> Value {
    let mut args = test_run_args(&rig.console, run, 0);
    args["docker"] = json!(true);
    let id = rig.console.queue_command("test-run", Some(args));
    for _ in 0..50 {
        pass(&rig.machines, 1);
        service.report().await;
        if let Some(result) = results_of(&rig.console, &id).pop() {
            return result;
        }
    }
    panic!("no result for {run}");
}

/// The Runner checks Docker in one machine: unknown until the check ends, which refuses nothing; a failure drops
/// the `cf-docker` sets and answers a test run with Docker `never:`, and is checked again after six hours; a pass
/// serves both again. The console's report says what the last check found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_docker_check_decides_whether_docker_jobs_and_runs_come_here() {
    let mut rig = Rig::new(3).await;
    let runner = docker_runner(&mut rig).await;
    runner.discover().await;
    assert!(serves_docker(&runner), "unknown Docker refused the cf-docker set");
    let status = serde_json::to_value(runner.status()).expect("a report");
    assert!(status["status"].get("docker").is_none(), "{}", status["status"]);
    let unknown = docker_run(&rig, &runner, "tr_aaaaaaaaaaaaaaaaaaaaaaaaaa").await;
    assert_eq!(unknown["ok"], json!(true), "{unknown}");
    // Its machine ends: the host's one Docker job's room is free for the check.
    rig.parts
        .vms
        .end(job_name("tr-aaaaaaaaaaaaaaaaaaaaaaaaaa"), End::Ok);
    pass(&rig.machines, 1);

    // The check boots, takes its slot and fails: the sandbox ended.
    let before = rig.parts.vms.started().len();
    runner.look_at_docker();
    let started = rig.parts.vms.started();
    assert_eq!(started.len(), before + 1, "no check machine started");
    let check = started.last().expect("the check").job.clone();
    assert!(started.last().is_some_and(|request| request.docker));
    runner.look_at_docker();
    assert_eq!(
        rig.parts.vms.started().len(),
        before + 1,
        "a second check while one stands"
    );
    pass(&rig.machines, 1);
    rig.parts.vms.console(
        &check,
        "cf-supervisor: the Docker sandbox ended: exit status: 70\n",
    );
    rig.parts.vms.end(&check, End::Failed);
    pass(&rig.machines, 1);
    runner.look_at_docker();
    runner.discover().await;
    assert!(!serves_docker(&runner), "a failed check left the cf-docker set");
    let status = serde_json::to_value(runner.status()).expect("a report");
    let docker = &status["status"]["docker"];
    assert_eq!(docker["ok"], json!(false), "{docker}");
    assert!(
        docker["checkedAt"].as_str().is_some_and(|at| at.ends_with('Z')),
        "{docker}"
    );
    assert!(
        docker["why"]
            .as_str()
            .is_some_and(|why| why.contains("exit status: 70")),
        "{docker}"
    );
    let refused = docker_run(&rig, &runner, "tr_bbbbbbbbbbbbbbbbbbbbbbbbbb").await;
    assert_eq!(refused["ok"], json!(false));
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("never: Docker does not start in this machine's job VMs (")),
        "{refused}"
    );

    // Not checked again before six hours, then again; a pass serves Docker again.
    let started = rig.parts.vms.started().len();
    rig.later(Duration::from_secs(6 * 3600 - 60));
    runner.look_at_docker();
    assert_eq!(rig.parts.vms.started().len(), started, "checked again too soon");
    rig.later(Duration::from_secs(60));
    runner.look_at_docker();
    assert_eq!(
        rig.parts.vms.started().len(),
        started + 1,
        "not checked again after six hours"
    );
    let again = rig.parts.vms.started().last().expect("the check").job.clone();
    pass(&rig.machines, 1);
    rig.parts.vms.end(&again, End::Ok);
    pass(&rig.machines, 1);
    runner.look_at_docker();
    runner.discover().await;
    assert!(
        serves_docker(&runner),
        "a passed check did not serve the cf-docker set again"
    );
    let status = serde_json::to_value(runner.status()).expect("a report");
    assert_eq!(status["status"]["docker"]["ok"], json!(true));
    assert!(status["status"]["docker"].get("why").is_none());
    let allowed = docker_run(&rig, &runner, "tr_cccccccccccccccccccccccccc").await;
    assert_eq!(allowed["ok"], json!(true), "{allowed}");
    // A pass is not checked again.
    rig.later(Duration::from_secs(48 * 3600));
    let started = rig.parts.vms.started().len();
    runner.look_at_docker();
    assert_eq!(rig.parts.vms.started().len(), started);
}

/// A test run with Docker that started before a check failed is answered `started` when the console sends it again,
/// as it does until it has the result: its machine stands, and a `never:` would have the console start it elsewhere too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_docker_run_that_started_before_a_failed_check_is_answered_started_again() {
    const RUN: &str = "tr_dddddddddddddddddddddddddd";
    let mut rig = Rig::new(3).await;
    rig.stop_pump().await;
    // Room for the run and the check's machine side by side.
    rig.machines = rig.parts.machines_with(|config| config.docker_max_jobs = 2);
    let runner = docker_runner(&mut rig).await;
    let started = docker_run(&rig, &runner, RUN).await;
    assert_eq!(started["ok"], json!(true), "{started}");

    runner.look_at_docker();
    let check = rig.parts.vms.started().last().expect("the check").job.clone();
    assert_ne!(check, job_name("tr-dddddddddddddddddddddddddd"));
    pass(&rig.machines, 1);
    rig.parts.vms.end(&check, End::Failed);
    pass(&rig.machines, 1);
    assert!(rig.machines.docker_check().is_some_and(|check| !check.ok));

    let again = docker_run(&rig, &runner, RUN).await;
    assert_eq!(again["ok"], json!(true), "{again}");
    assert_eq!(again["message"], json!("started tr-dddddddddddddddddddddddddd"));
}

/// No check boots while the host is paused: the budget gives it no slot. It boots once the pause is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_docker_check_boots_on_a_paused_host() {
    let mut rig = Rig::new(2).await;
    let runner = docker_runner(&mut rig).await;
    rig.machines.pause(true);
    runner.look_at_docker();
    assert!(
        rig.parts.vms.started().is_empty(),
        "a check booted on a paused host"
    );
    rig.machines.pause(false);
    runner.look_at_docker();
    assert_eq!(rig.parts.vms.started().len(), 1);
}

/// A failed Docker check holds across a restart: before any new check ends, the Runner that starts again serves no
/// `cf-docker` set, answers a run with Docker `never:` and reports the failure the one before found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_docker_check_holds_across_a_restart() {
    let mut rig = Rig::new(3).await;
    let runner = docker_runner(&mut rig).await;
    runner.look_at_docker();
    let check = rig.parts.vms.started().last().expect("the check").job.clone();
    pass(&rig.machines, 1);
    rig.parts.vms.console(
        &check,
        "cf-supervisor: the Docker sandbox ended: exit status: 70\n",
    );
    rig.parts.vms.end(&check, End::Failed);
    pass(&rig.machines, 1);
    let before = serde_json::to_value(runner.status()).expect("a report")["status"]["docker"].clone();
    assert_eq!(before["ok"], json!(false));
    drop(runner);

    rig.restart().await;
    let runner = docker_runner(&mut rig).await;
    let status = serde_json::to_value(runner.status()).expect("a report");
    assert_eq!(status["status"]["docker"], before, "the failure was forgotten");
    runner.discover().await;
    assert!(
        !serves_docker(&runner),
        "a cf-docker set before a new check passed"
    );
    let refused = docker_run(&rig, &runner, "tr_dddddddddddddddddddddddddd").await;
    assert!(
        refused["message"]
            .as_str()
            .is_some_and(|m| m.starts_with("never: Docker does not start")),
        "{refused}"
    );
}

/// Lets the Mac's time pass, a second at a time, until `done`.
async fn passes_until(machines: &TestMachines, what: &str, done: impl Fn() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        pass(machines, 1);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{what} never came");
}

/// A trusted host (`cf_trusted`): it makes no scale set whatever the workflows ask for, and asks for one Actions
/// runner with the label `trusted` at a time, outside any scale set, in the largest machine it has, whose machine
/// carries the rules each job is checked against before its first step. One that waited five minutes for a job is
/// removed. It takes no test run and says its class; the shared host beside it never serves a set with `trusted`,
/// from a list or a variable, and says no class.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trusted_host_asks_for_one_actions_runner_at_a_time_and_its_machine_carries_the_rules() {
    // Room for two of its largest machines: one at a time is the trusted host's own rule.
    let mut rig = Rig::new(20).await;
    rig.stop_pump().await;
    let rules = cf_trusted::Policy::new(
        json!({ "repository": REPO, "allow": { "deploy.yml": ["workflow_dispatch"] } }),
    );
    rig.machines = rig
        .parts
        .machines_with(|config| config.trusted = Some(rules.clone()));
    rig.github.set_workflow(
        REPO,
        "deploy.yml",
        "on: workflow_dispatch\njobs:\n  image:\n    runs-on: [self-hosted, cf-2g, cf-docker]\n    steps: []\n  listed:\n    runs-on: [cf-8g, trusted]\n    steps: []\n  deploy:\n    runs-on: ${{ vars.CI_RUNNER_TRUSTED || 'ubuntu-latest' }}\n    steps: []\n",
    );
    rig.github.set_variable(REPO, "CI_RUNNER_TRUSTED", "trusted");
    let (shared, _) = rig.service.survey().await;
    assert!(
        !shared.is_empty() && shared.iter().all(|line| !line.contains("trusted")),
        "{shared:?}"
    );
    let report = serde_json::to_value(rig.service.status()).expect("a report");
    assert_eq!(report.get("class"), None, "a shared host says no class");

    let trusted = service_with(
        &rig.github,
        &rig.console,
        &rig.parts,
        &rig.machines,
        &rig.token,
        &rig.battery,
        (
            &rig.outbox,
            None,
            &rig.settings,
            rig.folder.path(),
            rig.update_limit,
        ),
        |config| {
            config.policy.trusted = Some(rules.clone());
            // Left over from a shared host's file: a trusted one gives no Docker whatever it lists.
            config.policy.docker_repos = vec![REPO.into()];
        },
    );
    let (served, problems) = trusted.survey().await;
    assert_eq!(served, Vec::<String>::new());
    assert_eq!(problems, Vec::<String>::new());
    trusted.discover().await;
    trusted.look_at_queue().await;
    assert_eq!(trusted.sets().len(), 0, "no scale set");

    trusted.ask_trusted().await;
    let registered = rig.github.plain_runners();
    assert_eq!(registered.len(), 1, "{registered:?}");
    let (name, repo, labels) = registered[0].clone();
    assert_eq!((repo.as_str(), labels), (REPO, vec!["trusted".to_string()]));
    passes_until(&rig.machines, "its machine", || {
        !rig.parts.vms.started().is_empty()
    })
    .await;
    let started = rig.parts.vms.started();
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0].trusted, Some(rules));
    assert!(!started[0].docker);
    assert_eq!(
        started[0].memory_bytes,
        Size::G8.vm_bytes(),
        "the largest machine this host has"
    );

    passes_until(&rig.machines, "its machine waiting for a job", || {
        rig.machines
            .snapshot()
            .machines
            .iter()
            .any(|machine| machine.runner == name && machine.state == "idle")
    })
    .await;
    trusted.ask_trusted().await;
    assert_eq!(rig.github.plain_runners().len(), 1, "one at a time");
    assert_eq!(rig.parts.vms.started().len(), 1, "one at a time");

    let status = serde_json::to_value(trusted.status()).expect("a report");
    assert_eq!(status["class"], json!("trusted"));
    let commands = status["status"]["commands"].as_array().expect("commands");
    assert!(!commands.contains(&json!("test-run")), "{commands:?}");
    let id = rig.console.queue_command(
        "test-run",
        Some(test_run_args(&rig.console, "tr_tttttttttttttttttttttttttt", 0)),
    );
    let mut answer = None;
    for _ in 0..50 {
        pass(&rig.machines, 1);
        trusted.report().await;
        answer = results_of(&rig.console, &id).pop();
        if answer.is_some() {
            break;
        }
    }
    let answer = answer.expect("the test run is answered");
    assert_eq!(answer["ok"], json!(false), "{answer}");
    assert!(
        answer["message"]
            .as_str()
            .is_some_and(|message| message.starts_with("never: a trusted host")),
        "{answer}"
    );
    assert_eq!(rig.parts.vms.started().len(), 1, "no test run started");

    // Its job went elsewhere or was cancelled: after five minutes of waiting it is removed.
    rig.later(Duration::from_secs(4 * 60));
    trusted.ask_trusted().await;
    for _ in 0..5 {
        pass(&rig.machines, 1);
        trusted.sweep().await;
    }
    assert_eq!(rig.github.removed_at_request(), Vec::<String>::new());
    rig.later(Duration::from_secs(60));
    trusted.ask_trusted().await;
    for _ in 0..100 {
        if rig.github.removed_at_request() == [name.clone()] {
            break;
        }
        pass(&rig.machines, 1);
        trusted.sweep().await;
    }
    assert_eq!(rig.github.removed_at_request(), [name]);
}

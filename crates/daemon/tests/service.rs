//! The whole service against a fake GitHub and a fake console, with real sockets and the real clients: a job from
//! GitHub's queue to the console's page, what GitHub and the console do when they fail, and a Runner that starts
//! again. Only the vm-hosts and the Mac's readings are stand-ins.

mod support;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cf_console_client::{Client, Enroll, JobEvent, OUTBOX_CAPACITY, Outbox, enroll};
use cf_daemon::names::job_name;
use cf_daemon::testing::{CrashingStore, FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for};
use cf_daemon::{
    Console, HostExtras, JobReport, Nudge, OutboxReports, RESULT_WAIT, Reports, Roster, Service,
    ServiceConfig, ServiceParts,
};
use cf_fake_console::FakeConsole;
use cf_fake_github::harness::{REPO, TestClock, app, fast, until};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, JobState, Options};
use cf_ghapp::{Clock as _, Http, RetryPolicy};
use cf_host_macos::{Battery, FakeHost};
use cf_scaleset::ScaleSetClient;
use cf_secret::Secret;
use cf_vmhost::End;
use proptest::prelude::*;
use serde_json::{Value, json};

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
    /// Lets time pass on the Mac and runs the service's rounds, as its loop does.
    pump: Option<(Arc<AtomicBool>, tokio::task::JoinHandle<()>)>,
}

fn http() -> Http {
    Http::new(Duration::from_secs(5), fast())
}

impl Rig {
    /// A Mac with room for `machines` machines, enrolled at the console, serving [`REPO`] with one workflow.
    async fn new(machines: u32) -> Rig {
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
            settings: settings_for(machines),
            nudges: Arc::new(AtomicUsize::new(0)),
            boot: "start-1".into(),
            bell: Arc::new(Nudge::default()),
        };
        parts
            .reports
            .forward_to(Arc::new(Gated(dead.clone(), OutboxReports(outbox.clone()))));
        let machines = parts.machines(None);
        let battery = Arc::new(Mutex::new(None));
        let mut rig = Rig {
            service: Arc::new(service(
                &github,
                &console,
                &parts,
                &machines,
                &outbox,
                &enrolled.token,
                &battery,
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
            pump: None,
        };
        rig.pump();
        rig
    }

    fn pump(&mut self) {
        let stop = Arc::new(AtomicBool::new(false));
        let (stopped, machines, service) = (stop.clone(), self.machines.clone(), self.service.clone());
        let task = tokio::spawn(async move {
            while !stopped.load(Ordering::Relaxed) {
                pass(&machines, 1);
                service.sweep().await;
                service.report().await;
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
        });
        self.pump = Some((stop, task));
    }

    async fn stop_pump(&mut self) {
        if let Some((stop, task)) = self.pump.take() {
            stop.store(true, Ordering::Relaxed);
            task.await.expect("pump");
        }
    }

    /// The Runner's process dies and a new one starts: nothing is closed at GitHub, the vm-hosts go on. One that died
    /// at a write already ([`Rig::crash_at`]) is only started again.
    async fn restart(&mut self) {
        self.stop_pump().await;
        self.service.kill();
        if self.store.crashed() {
            // The listeners end at their next wait; until then GitHub and the vm-hosts keep refusing them.
            tokio::time::sleep(Duration::from_millis(50)).await;
            self.parts.vms.fail_start(0);
            self.parts.vms.ignore_stops(false);
        }
        self.store.revive();
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
            &self.outbox,
            &self.token,
            &self.battery,
        ));
        pass(&self.machines, 61);
        self.pump();
    }

    /// The Runner dies at its `writes`th write from now, before or after it is `kept`: from that moment nothing it does
    /// reaches GitHub, the vm-hosts or the console's outbox, as for a process that is gone.
    fn crash_at(&self, writes: usize, kept: bool) {
        let (dead, github, vms) = (self.dead.clone(), self.github.clone(), self.parts.vms.clone());
        self.store.crash_at(writes, kept, move || {
            dead.store(true, Ordering::Relaxed);
            github.set_down(true);
            vms.fail_start(usize::MAX);
            vms.ignore_stops(true);
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
        until("the Mac to offer room", || self.github.offers_room()).await;
        self.github.queue_job(REPO, &["self-hosted", "cf-2g"], "build")
    }

    /// The Actions runners whose machine stands.
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
        until("a machine for the job", || self.standing().len() > before.len()).await;
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
    outbox: &Arc<Outbox>,
    token: &Secret<String>,
    battery: &Arc<Mutex<Option<Battery>>>,
) -> TestService {
    let battery = battery.clone();
    let mut config = ServiceConfig::new(
        MACHINE,
        "Build-Mac-2.local",
        "Apple M5 (Mac17,3)",
        "0.1.0+446125c0a1b2",
    );
    config.poll_timeout = Duration::from_secs(5);
    config.sweep_again = Duration::from_millis(50);
    config.backoff = (
        Duration::from_millis(20),
        Duration::from_millis(100),
        Duration::from_secs(60),
    );
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
                "POST /svc/_apis/runtime/runnerscalesets/1/sessions",
                || Reply::text(401, "bad credentials"),
                2,
            ),
        ),
        (
            "409: a session of before is still open",
            answers(
                "POST /svc/_apis/runtime/runnerscalesets/1/sessions",
                || Reply::status_json(409, &json!({"message": "exists"})),
                3,
            ),
        ),
        (
            "429 on the long poll",
            answers(
                "GET /svc/_apis/runtime/runnerscalesets/1/messages",
                || Reply::text(429, "slow down").with_header("retry-after", "0"),
                3,
            ),
        ),
        (
            "5xx on the long poll",
            answers(
                "GET /svc/_apis/runtime/runnerscalesets/1/messages",
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
                "POST /svc/_apis/runtime/runnerscalesets?",
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
            Box::new(|github| github.lose_answer("GET /svc/_apis/runtime/runnerscalesets/1/messages", 1)),
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
    let update = rig.console.queue_command("update", None);
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
                r["ok"] == json!(false) && r["message"] == json!("this Runner does not do `update` yet")
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
    let other = rig.console.queue_command("restart", None);
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
        json!("this Runner does not do `restart` yet")
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
        &rig.outbox,
        &Secret::new("not-this-macs-token".to_string()),
        &rig.battery,
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
    let report = |rig: &Rig| {
        pass(&rig.machines, 1);
        serde_json::to_value(rig.service.status()).expect("a report")
    };
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
    let mut rig = Rig::new(2).await;
    rig.service.discover().await;
    until("the scale set's session", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    rig.stop_pump().await;
    let status = |rig: &Rig| {
        pass(&rig.machines, 1);
        serde_json::to_value(rig.service.status()).expect("a report")["status"].clone()
    };
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
    /// A job is cancelled, waiting at GitHub or assigned here.
    Cancelled(u8),
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
    /// Five minutes pass for GitHub and the Runner: an assignment no Actions runner took is taken back, and an idle
    /// Actions runner is surplus.
    FiveMinutes,
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
        1 => any::<u8>().prop_map(Happens::Cancelled),
        1 => any::<u8>().prop_map(Happens::TakenElsewhere),
        2 => any::<bool>().prop_map(Happens::Memory),
        2 => Just(Happens::GithubLooks),
        1 => any::<u8>().prop_map(Happens::SessionLost),
        2 => (any::<u8>(), prop_oneof![Just(Lost::Start), Just(Lost::End), Just(Lost::Both)])
            .prop_map(|(i, lost)| Happens::LostWith(i, lost)),
        1 => any::<u8>().prop_map(Happens::AckFails),
        1 => Just(Happens::Restart),
        2 => (any::<u8>(), any::<bool>()).prop_map(|(at, kept)| Happens::Crash(at, kept)),
        2 => any::<bool>().prop_map(Happens::RestStale),
        2 => any::<u8>().prop_map(Happens::RestFails),
        1 => Just(Happens::FiveMinutes),
        1 => Just(Happens::Uninstall),
        3 => (1u8..30).prop_map(Happens::Wait),
    ]
}

/// How soon, in these tests' timings, GitHub's look at its queue moves a job this Mac serves that waits while a
/// machine of its size fits: the Mac's polls show the room within two of them (150 ms each here, the long poll in
/// service), and the assignment and the start follow.
const STARTS_WITHIN: Duration = Duration::from_secs(3);

/// How soon a job whose start and end went with a session is at the console once its machine is gone: the wait for
/// GitHub's word ([`cf_daemon::RESULT_WAIT`], two minutes of the Mac's clock, which the rig runs at a minute a
/// second), and a sweep.
const HEARD_WITHIN: Duration = Duration::from_secs(10);

/// Failed acknowledgements that outlast the HTTP retries ([`fast`]: two) and fail the listener once.
const PAST_RETRIES: usize = 3;

const SECOND: &str = "org/second";

fn pick<T: Clone>(items: &[T], n: u8) -> Option<T> {
    (!items.is_empty()).then(|| items[usize::from(n) % items.len()].clone())
}

/// The Runner died, or dies: it starts again, and opens every scale set's session anew.
async fn start_again(rig: &mut Rig, sets: &[i64], causes: &mut [usize]) {
    rig.restart().await;
    for (at, set) in sets.iter().enumerate() {
        rig.github.lose_session(*set);
        causes[at] += 1;
    }
    rig.service.discover().await;
}

/// Machines started may never outnumber the jobs GitHub handed to this Mac, each hand-out counted.
fn one_machine_per_assignment(rig: &Rig) -> Result<(), TestCaseError> {
    let (started, assigned) = (rig.parts.vms.started().len(), rig.github.assignments());
    prop_assert!(
        started <= assigned,
        "{started} machines for {assigned} jobs handed to this Mac"
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
    for step in script {
        if rig.store.crashed() {
            start_again(&mut rig, &sets, &mut causes).await;
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
            Happens::Cancelled(n) => {
                let waiting = in_state(|s| matches!(s, JobState::Available | JobState::Acquired(_)));
                if let Some(job) = pick(&waiting, n) {
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
            Happens::Restart => start_again(&mut rig, &sets, &mut causes).await,
            Happens::Crash(at, kept) => rig.crash_at(1 + usize::from(at % 8), kept),
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
            Happens::FiveMinutes => {
                rig.github.pass(Duration::from_secs(301));
                rig.later(Duration::from_secs(301));
            }
            Happens::Uninstall => uninstall = true,
            Happens::Wait(tens) => {
                tokio::time::sleep(Duration::from_millis(u64::from(tens) * 10)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        one_machine_per_assignment(&rig)?;
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
        start_again(&mut rig, &sets, &mut causes).await;
    }
    // A crash that has not come does not come now.
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
        all_heard(&rig, &ran).await?;
        rig.stop_pump().await;
        rig.service.kill();
        return Ok(());
    }

    // Room for every machine: what waits at GitHub moves at each of GitHub's looks, until the slots are full.
    rig.machines
        .with_host(|host| host.set(|reading| reading.available_bytes = roomy));
    let slots = usize::try_from(slots).unwrap_or(usize::MAX);
    let waiting = |rig: &Rig| {
        jobs.iter()
            .filter(|&&job| rig.github.job_state(job) == Some(JobState::Available))
            .count()
    };
    loop {
        let (before, standing) = (waiting(&rig), rig.standing().len());
        if before == 0 || standing >= slots {
            break;
        }
        // Two polls that show the room, then GitHub looks.
        tokio::time::sleep(Duration::from_millis(400)).await;
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
    all_heard(&rig, &ran).await?;
    rig.stop_pump().await;
    rig.service.kill();
    Ok(())
}

/// Every job that ran here reaches the console, its start and its end, each once.
async fn all_heard(rig: &Rig, ran: &BTreeSet<String>) -> Result<(), TestCaseError> {
    let heard_from = std::time::Instant::now();
    loop {
        let unheard: Vec<&String> = ran
            .iter()
            .filter(|runner| heard(rig, runner) != (true, true))
            .collect();
        if unheard.is_empty() {
            break;
        }
        prop_assert!(
            heard_from.elapsed() < HEARD_WITHIN,
            "jobs that ran here and never reached the console: {unheard:?}"
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
    for runner in &started {
        prop_assert!(
            ran.contains(*runner),
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
    #![proptest_config(ProptestConfig { cases: 24, failure_persistence: None, ..ProptestConfig::default() })]

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

/// GPT, scaler.rs:525: a session opens with counts that name a running job, and the job ends right after them: at
/// the Runner's first look at GitHub once the session is open, the REST view or the first poll. The counts are from
/// before the end: no second machine starts for a job that is over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_that_ends_as_a_session_opens_gets_no_second_machine_from_the_opening_counts() {
    let rig = Rig::new(2).await;
    rig.service.discover().await;
    let runner = rig.job_running().await;
    until("the start at the console", || heard(&rig, &runner).0).await;
    let set = rig.github.scale_sets()[0].id;
    for first_look in [
        "GET /repos/org/app/actions/runners/".to_string(),
        format!("GET /svc/_apis/runtime/runnerscalesets/{set}/messages"),
    ] {
        let runner = runner.clone();
        rig.github.before(&first_look, move |github| {
            if github.scale_sets().iter().any(|s| s.has_session) {
                github.complete_job(&runner, "succeeded");
            }
        });
    }
    rig.github.lose_session(set);
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

/// GPT, scaler.rs:582: a job is cancelled before its Actions runner took it, the surplus Actions runner is removed
/// at GitHub, and the Runner dies before the machine is gone. The Runner that starts next finds the registration
/// gone: its own removal, not a job.
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
    rig.restart().await;
    let set = rig.github.scale_sets()[0].id;
    rig.github.lose_session(set);
    rig.service.discover().await;
    until("a session again", || {
        rig.github.scale_sets().iter().any(|s| s.has_session)
    })
    .await;
    until(
        "the next Runner to finish the removal it found written down",
        || removals(&rig) > removed_before,
    )
    .await;
    until("the machine stopped once its registration is gone", || {
        stops(&rig) > stopped_before
    })
    .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
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

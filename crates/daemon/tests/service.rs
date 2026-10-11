//! The whole service against a fake GitHub and a fake console, with real sockets and the real clients: a job from
//! GitHub's queue to the console's page, what GitHub and the console do when they fail, and a Runner that starts
//! again. Only the vm-hosts and the Mac's readings are stand-ins.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use cf_console_client::{Client, Enroll, OUTBOX_CAPACITY, Outbox, enroll};
use cf_daemon::names::job_name;
use cf_daemon::testing::{FakeVms, ReportLog, TestMachines, TestParts, pass, settings_for};
use cf_daemon::{
    Console, HostExtras, MemoryStore, Nudge, OutboxReports, Roster, Service, ServiceConfig, ServiceParts,
};
use cf_fake_console::FakeConsole;
use cf_fake_github::harness::{REPO, TestClock, app, fast, until};
use cf_fake_github::httpd::Reply;
use cf_fake_github::{FakeGithub, Options};
use cf_ghapp::{Http, RetryPolicy};
use cf_host_macos::FakeHost;
use cf_secret::Secret;
use cf_vmhost::End;
use serde_json::{Value, json};

const MACHINE: &str = "mac-b";
const WORKFLOW: &str = "on: push\njobs:\n  build:\n    runs-on: [self-hosted, cf-2g]\n    steps: []\n";

type TestService = Service<FakeHost, FakeVms>;

struct Rig {
    github: FakeGithub,
    console: FakeConsole,
    parts: TestParts,
    machines: Arc<TestMachines>,
    service: Arc<TestService>,
    outbox: Arc<Outbox>,
    token: Secret<String>,
    folder: tempfile::TempDir,
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
        let parts = TestParts {
            vms: FakeVms::default(),
            store: Arc::new(MemoryStore::default()),
            roster: Arc::new(Roster::new()),
            clock: Arc::new(TestClock::default()),
            reports: Arc::new(ReportLog::default()),
            settings: settings_for(machines),
            nudges: Arc::new(AtomicUsize::new(0)),
            boot: 1,
            bell: Arc::new(Nudge::default()),
        };
        parts.reports.forward_to(Arc::new(OutboxReports(outbox.clone())));
        let machines = parts.machines(None);
        let mut rig = Rig {
            service: Arc::new(service(
                &github,
                &console,
                &parts,
                &machines,
                &outbox,
                &enrolled.token,
            )),
            github,
            console,
            parts,
            machines,
            outbox,
            token: enrolled.token,
            folder,
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

    /// The Runner's process dies and a new one starts: nothing is closed at GitHub, the vm-hosts go on.
    async fn restart(&mut self) {
        self.stop_pump().await;
        self.service.kill();
        let host = self.machines.with_host(|host| host.clone());
        self.parts.roster = Arc::new(Roster::new());
        self.parts.bell = Arc::new(Nudge::default());
        self.outbox = Arc::new(Outbox::open(
            self.folder.path().join("outbox.json"),
            OUTBOX_CAPACITY,
        ));
        self.parts
            .reports
            .forward_to(Arc::new(OutboxReports(self.outbox.clone())));
        self.machines = self.parts.restored(host);
        self.service = Arc::new(service(
            &self.github,
            &self.console,
            &self.parts,
            &self.machines,
            &self.outbox,
            &self.token,
        ));
        pass(&self.machines, 61);
        self.pump();
    }

    fn queue(&self) -> i64 {
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
        self.queue();
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

fn service(
    github: &FakeGithub,
    console: &FakeConsole,
    parts: &TestParts,
    machines: &Arc<TestMachines>,
    outbox: &Arc<Outbox>,
    token: &Secret<String>,
) -> TestService {
    let mut config = ServiceConfig::new(MACHINE, "Build-Mac-2.local", "Mac16,12", "0.1.0");
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
        extras: Box::new(|| HostExtras {
            load: [1.5, 1.0, 0.5],
            disk_total_bytes: 2000 << 30,
            swap_total_bytes: 0,
            cpu_percent: 12.0,
            boot: Some(1_790_000_000),
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
    rig.queue();
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
    rig.queue();
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

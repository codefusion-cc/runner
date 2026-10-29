//! The console's `test-run` command on the machines' side: which arguments are taken, when the Mac says it is busy,
//! how a run waits for room and goes before CI jobs that wait, and the machine itself: started with the agent and
//! the run's address alone, its job started when it is up, never taken for an idle Actions runner, its slot given
//! back when it ends, adopted by a Runner that starts again, and never a word to GitHub about it.

mod support;

use std::time::{Duration, Instant};

use cf_budget::{Size, Turn};
use cf_daemon::names::job_name;
use cf_daemon::test_run::{self, Admission, TestRun, Waited};
use cf_daemon::testing::pass;
use cf_daemon::{Claim, EndKind, EventKind, JobReport, Slots, job_event};
use cf_guest_proto::Workload;
use cf_vmhost::End;
use serde_json::{Value, json};
use support::world::World;

const CONSOLE: &str = "https://console.example.org";
const RUN: &str = "tr_abcdefghijklmnopqrstuvwx23";
const MACHINE: &str = "tr-abcdefghijklmnopqrstuvwx23";

fn args() -> Value {
    json!({
        "run": RUN,
        "url": format!("{CONSOLE}/agent/runners/runs/{RUN}"),
        "token": format!("cfrt_{}", "a1_-".repeat(10) + "xyz"),
        "size": "cf-2g",
        "docker": false,
        "timeoutS": 1800,
        "waitS": 0,
        "repo": "codefusion-cc/codefusion-console",
        "label": "feat/x: npm run test:browser"
    })
}

fn with(changes: Value) -> Value {
    let mut args = args();
    for (key, value) in changes.as_object().expect("an object") {
        if value.is_null() {
            args.as_object_mut().expect("an object").remove(key);
        } else {
            args[key] = value.clone();
        }
    }
    args
}

fn run(changes: Value) -> TestRun {
    TestRun::from_args(Some(&with(changes)), CONSOLE).expect("a test run")
}

fn reports_of(w: &World, runner: &str) -> Vec<JobReport> {
    w.reports().into_iter().filter(|r| r.runner == runner).collect()
}

/// Starts a test run on this Mac as the service does, and says its machine.
fn started(w: &World, run: &TestRun) -> String {
    let other = w.m.test_run_on();
    let Admission::Start(ticket) = test_run::admit(&*w.m, run, other.as_deref()) else {
        panic!("no slot for the test run");
    };
    w.m.start_test_run(ticket, run).expect("the machine starts")
}

#[test]
fn every_argument_out_of_bounds_is_refused_and_the_bounds_themselves_are_taken() {
    let refused = [
        json!({"run": "tr_abcdefghijklmnopqrstuvwx2"}),
        json!({"run": "tr_abcdefghijklmnopqrstuvwx231"}),
        json!({"run": "tr_ABCDEFGHIJKLMNOPQRSTUVWX23"}),
        json!({"run": "tr_abcdefghijklmnopqrstuvwx01"}),
        json!({"run": "tx_abcdefghijklmnopqrstuvwx23"}),
        json!({"run": 7}),
        json!({"run": null}),
        json!({"url": format!("https://evil.example/agent/runners/runs/{RUN}")}),
        json!({"url": format!("{CONSOLE}.evil.example/agent/runners/runs/{RUN}")}),
        json!({"url": format!("{CONSOLE}/agent/runners/runs/tr_abcdefghijklmnopqrstuvwxyz")}),
        json!({"url": format!("{CONSOLE}/agent/runners/runs/{RUN}/../other")}),
        json!({"url": format!("http://console.example.org/agent/runners/runs/{RUN}")}),
        json!({"token": format!("cfrt_{}", "a".repeat(42))}),
        json!({"token": format!("cfrt_{}", "a".repeat(44))}),
        json!({"token": format!("cfrt_{}+", "a".repeat(42))}),
        json!({"token": format!("cfr_{}", "a".repeat(44))}),
        json!({"size": "cf-3g"}),
        json!({"size": "self-hosted"}),
        json!({"size": "CF-2G"}),
        json!({"docker": "yes"}),
        json!({"timeoutS": 59}),
        json!({"timeoutS": 7201}),
        json!({"timeoutS": "1800"}),
        json!({"timeoutS": 60.5}),
        json!({"timeoutS": null}),
        json!({"waitS": -1}),
        json!({"waitS": 3601}),
        json!({"repo": ""}),
        json!({"repo": "o/".to_string() + &"r".repeat(199)}),
        json!({"repo": "org/app\n"}),
        json!({"label": "é".repeat(301)}),
        json!({"label": "feat/x\u{1b}[31m"}),
        json!({"label": "line\nline"}),
    ];
    for change in refused {
        let why = TestRun::from_args(Some(&with(change.clone())), CONSOLE)
            .expect_err(&format!("{change} was taken"));
        // Said in a sentence, and never with the value of what was refused, which may be a token.
        assert!(!why.is_empty() && !why.contains("aaaaaaaaaa"), "{change}: {why}");
    }
    assert!(TestRun::from_args(None, CONSOLE).is_err());
    assert!(TestRun::from_args(Some(&json!([1])), CONSOLE).is_err());
    // A Runner with no console takes no run: there is nowhere it could be.
    assert!(TestRun::from_args(Some(&args()), "").is_err());

    for change in [
        json!({"timeoutS": 60, "waitS": 0}),
        json!({"timeoutS": 7200, "waitS": 3600}),
        json!({"repo": "o/".to_string() + &"r".repeat(198)}),
        json!({"label": "é".repeat(300)}),
        json!({"label": ""}),
        json!({"docker": null}),
        json!({"size": "cf-8g", "docker": true}),
    ] {
        TestRun::from_args(Some(&with(change.clone())), CONSOLE)
            .unwrap_or_else(|why| panic!("{change} was refused: {why}"));
    }
    // The console's address as configured, with or without its slash.
    assert!(TestRun::from_args(Some(&args()), &format!("{CONSOLE}/")).is_ok());

    let run = run(json!({"timeoutS": 7200}));
    assert_eq!(run.runner(), MACHINE);
    assert_eq!(
        job_name(&run.runner()).len(),
        26,
        "a vm-host takes 32 characters at most"
    );
    assert_eq!(
        run.deadline(Duration::from_secs(6 * 3600)),
        Duration::from_secs(7200 + 900)
    );
    assert_eq!(
        run.deadline(Duration::from_secs(3600)),
        Duration::from_secs(3600),
        "never past what any job may run"
    );
    assert!(
        !format!("{run:?}").contains("cfrt_"),
        "the token is in the debug output"
    );
}

#[tokio::test]
async fn a_closed_host_or_one_with_a_test_run_on_it_is_busy_and_holds_nothing() {
    let w = World::new(3);
    let run = run(json!({"waitS": 60}));

    w.m.pause(true);
    let Admission::Busy(why) = test_run::admit(&*w.m, &run, None) else {
        panic!("a paused Mac took it");
    };
    assert!(why.contains("paused"), "{why}");
    w.m.pause(false);

    w.m.hold_for_restart(true);
    assert!(matches!(test_run::admit(&*w.m, &run, None), Admission::Busy(_)));
    w.m.hold_for_restart(false);
    assert_eq!(w.slots(), 0);

    let machine = started(&w, &run);
    assert_eq!(w.m.test_run_on().as_deref(), Some(machine.as_str()));
    let other = w.m.test_run_on();
    let Admission::Busy(why) = test_run::admit(&*w.m, &run, other.as_deref()) else {
        panic!("a second test run was taken");
    };
    assert!(why.contains(MACHINE), "{why}");
    assert_eq!(w.slots(), 1, "the second one holds nothing");
    assert_eq!(w.m.snapshot().waiting, 0);
}

#[tokio::test]
async fn no_room_and_no_wait_is_busy_and_leaves_no_place_in_line() {
    let mut w = World::new(1);
    let ci = w.running().await;
    let Admission::Busy(why) = test_run::admit(&*w.m, &run(json!({"waitS": 0})), None) else {
        panic!("a full Mac with no wait did not say busy");
    };
    assert!(why.starts_with("no room now"), "{why}");
    assert_eq!(w.m.snapshot().waiting, 0, "a place in line was left");

    // The job that runs ends: its room is nobody's, and nothing starts on it.
    w.parts.vms.end(job_name(&ci), End::Ok);
    w.github(EventKind::Completed, &ci, "succeeded");
    pass(&w.m, 3);
    assert_eq!(w.slots(), 0);
    assert_eq!(w.parts.vms.started().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_waits_starts_when_room_comes() {
    let mut w = World::new(1);
    let ci = w.running().await;
    let run = run(json!({"waitS": 60}));
    let Admission::Waits(ticket) = test_run::admit(&*w.m, &run, None) else {
        panic!("a full Mac with a wait gave no place in line");
    };
    let m = w.m.clone();
    let waits =
        tokio::spawn(async move { test_run::wait_turn(&*m, ticket, Duration::from_secs(60), || true).await });
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert!(!waits.is_finished(), "it did not wait for the job that runs");

    w.parts.vms.end(job_name(&ci), End::Ok);
    w.github(EventKind::Completed, &ci, "succeeded");
    pass(&w.m, 2);
    assert_eq!(
        waits.await.expect("the wait"),
        Waited::Ready,
        "the room went to nobody"
    );
    assert_eq!(w.m.start_test_run(ticket, &run).as_deref(), Ok(MACHINE));
    assert_eq!(w.slots(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wait_that_runs_out_is_withdrawn() {
    let mut w = World::new(1);
    let ci = w.running().await;
    let run = run(json!({"waitS": 1}));
    let Admission::Waits(ticket) = test_run::admit(&*w.m, &run, None) else {
        panic!("no place in line");
    };
    let began = Instant::now();
    assert_eq!(
        test_run::wait_turn(&*w.m, ticket, Duration::from_secs(run.wait_s), || true).await,
        Waited::RanOut
    );
    assert!(began.elapsed() >= Duration::from_secs(1), "it gave up early");
    assert!(
        began.elapsed() < Duration::from_secs(3),
        "it waited past its time"
    );
    assert_eq!(w.m.snapshot().waiting, 0, "its place in line was left");

    // The room that comes later is nobody's: no machine starts on it, no slot is held.
    w.parts.vms.end(job_name(&ci), End::Ok);
    w.github(EventKind::Completed, &ci, "succeeded");
    pass(&w.m, 3);
    assert_eq!(w.slots(), 0);
    assert!(!w.m.ready(ticket));
}

#[test]
fn a_size_the_host_never_runs_is_never() {
    // Room for one cf-2g machine at most: a cf-8g never fits, whatever ends.
    let w = World::new(1);
    let Admission::Never(why) = test_run::admit(&*w.m, &run(json!({"size": "cf-8g", "waitS": 60})), None)
    else {
        panic!("it was not refused for good");
    };
    assert!(!why.is_empty());
    assert_eq!(w.slots(), 0);
    assert_eq!(w.m.snapshot().waiting, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_urgent_test_run_goes_before_a_ci_job_that_waited_longer() {
    let mut w = World::new(1);
    let ci = w.running().await;
    // A job of a repository that never started one here waits first.
    let Claim::Waits { ticket: waiting, .. } = w.m.claim_in_turn(
        Size::G2,
        None,
        Turn {
            urgent: false,
            repo: "org/other".into(),
            refusable: false,
        },
    ) else {
        panic!("the CI job got room");
    };
    pass(&w.m, 1);
    let admission = test_run::admit(&*w.m, &run(json!({"waitS": 60})), None);
    let Admission::Waits(run_ticket) = admission else {
        panic!("no place in line: {admission:?}");
    };
    w.parts.vms.end(job_name(&ci), End::Ok);
    w.github(EventKind::Completed, &ci, "succeeded");
    pass(&w.m, 2);
    assert!(w.m.ready(run_ticket), "the test run did not go first");
    assert!(!w.m.ready(waiting), "both got the one room");
}

#[tokio::test]
async fn the_machine_runs_the_agent_with_the_runs_address_alone_and_its_slot_goes_back_when_it_ends() {
    let mut w = World::new(2);
    let run = run(json!({"docker": true, "timeoutS": 600}));
    let machine = started(&w, &run);
    assert_eq!(machine, MACHINE);
    let request = w.parts.vms.started().pop().expect("a machine");
    assert_eq!(request.job, job_name(MACHINE));
    assert_eq!(request.workload, test_run::workload());
    let Workload::Command { argv } = &request.workload else {
        panic!("not a command")
    };
    assert_eq!(argv[..3], ["/usr/local/bin/node", "--input-type=module", "-e"]);
    assert_eq!(argv[3], test_run::AGENT);
    let env: Vec<(&str, &str)> = request
        .env
        .iter()
        .map(|(k, v)| (k.as_str(), v.expose().as_str()))
        .collect();
    assert_eq!(
        env,
        [
            ("CF_RUN_URL", run.url.as_str()),
            ("CF_RUN_TOKEN", run.token.expose().as_str()),
            ("CF_RUN_TIMEOUT_S", "600"),
        ]
    );
    assert_eq!(request.deadline_s, 600 + 900);
    assert!(request.docker);
    assert_eq!(request.memory_bytes, Size::G2.vm_bytes());
    let record = w.parts.store.load().pop().expect("a record");
    assert!(record.unregistered && record.runner_id.is_none());
    assert!(
        !serde_json::to_string(&record).expect("json").contains("cfrt_"),
        "the token is in the record"
    );

    // Up: its job starts at once, with what the console shows of it.
    pass(&w.m, 1);
    let reports = reports_of(&w, MACHINE);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].id, format!("{MACHINE}/started"));
    let event = job_event(&reports[0]);
    assert_eq!(event.repo.as_deref(), Some("codefusion-cc/codefusion-console"));
    assert_eq!(event.job.as_deref(), Some("feat/x: npm run test:browser"));
    assert_eq!(event.workflow.as_deref(), Some("test-remote"));
    assert_eq!(event.event.as_deref(), Some("test-run"));
    assert_eq!(event.run_id, None);
    let status = &w.m.snapshot().machines[0];
    assert_eq!((status.state, status.size), ("running", Some(Size::G2)));
    assert_eq!(
        status.github.as_ref().map(|job| job.job.as_str()),
        Some("feat/x: npm run test:browser")
    );

    // Long past an idle Actions runner's time and GitHub's wait for a result: nothing asks GitHub anything of it
    // and nothing stops it.
    for _ in 0..20 {
        pass(&w.m, 60);
        assert!(w.m.registrations_to_remove().is_empty());
    }
    assert!(w.parts.vms.stopped().is_empty() && w.parts.vms.killed().is_empty());
    assert_eq!(w.m.snapshot().machines[0].state, "running");

    // The agent is done and the machine powers off: the slot goes back and the run is reported as a success.
    w.parts.vms.end(job_name(MACHINE), End::Ok);
    pass(&w.m, 1);
    assert_eq!(w.slots(), 0);
    assert_eq!(w.m.test_run_on(), None);
    let reports = reports_of(&w, MACHINE);
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[1].id, format!("{MACHINE}/completed"));
    assert_eq!(reports[1].end, Some(EndKind::Success));
    assert_eq!(job_event(&reports[1]).result.as_deref(), Some("success"));
    assert!(reports[1].machine.boot_ms.is_some());
    w.assert_nothing_left();
    assert_eq!(reports_of(&w, MACHINE).len(), 2, "reported twice");
}

#[tokio::test]
async fn a_machine_that_dies_or_never_comes_up_is_reported_with_how_it_ended() {
    let w = World::new(2);
    let run = run(json!({}));
    started(&w, &run);
    pass(&w.m, 1);
    w.parts.vms.die(job_name(MACHINE));
    pass(&w.m, 1);
    let reports = reports_of(&w, MACHINE);
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[1].end, Some(EndKind::GuestLost));
    assert_eq!(job_event(&reports[1]).result.as_deref(), Some("guest-lost"));
    assert_eq!(w.slots(), 0);

    // One that never comes up is started and ended at once for the console, which waits for it.
    let w = World::new(2);
    w.parts.vms.slow_boot(true);
    started(&w, &run);
    pass(&w.m, 1);
    assert!(reports_of(&w, MACHINE).is_empty(), "started before it was up");
    pass(&w.m, 120);
    let reports = reports_of(&w, MACHINE);
    assert_eq!(
        reports.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
        [format!("{MACHINE}/started"), format!("{MACHINE}/completed")]
    );
    assert_eq!(reports[1].end, Some(EndKind::StartFailed));
    assert_eq!(w.slots(), 0);
    assert!(w.m.registrations_to_remove().is_empty());
}

#[tokio::test]
async fn a_runner_that_starts_again_adopts_the_run_without_github_and_leaks_no_slot() {
    let mut w = World::new(2);
    started(&w, &run(json!({})));
    pass(&w.m, 1);
    w.restart();
    pass(&w.m, 1);
    assert_eq!(w.slots(), 1, "the adopted machine's memory is not counted");
    assert_eq!(w.m.test_run_on().as_deref(), Some(MACHINE));
    assert!(
        w.parts.roster.get(MACHINE).is_none(),
        "the scalers count it as an Actions runner"
    );
    assert!(w.m.registrations_to_remove().is_empty());
    assert_eq!(reports_of(&w, MACHINE).len(), 1, "its start was reported again");
    pass(&w.m, 15 * 60);
    assert!(w.parts.vms.stopped().is_empty(), "the adopted run was stopped");

    w.parts.vms.end(job_name(MACHINE), End::Ok);
    pass(&w.m, 1);
    assert_eq!(w.slots(), 0);
    let ends: Vec<_> = reports_of(&w, MACHINE)
        .into_iter()
        .filter_map(|r| r.end)
        .collect();
    assert_eq!(ends, [EndKind::Success]);
    w.assert_nothing_left();

    // A machine that died while no Runner watched: ended for the console, and no slot is held for it.
    let mut w = World::new(2);
    started(&w, &run(json!({})));
    pass(&w.m, 1);
    w.parts.vms.die(job_name(MACHINE));
    w.restart();
    pass(&w.m, 1);
    assert_eq!(w.slots(), 0);
    let ends: Vec<_> = reports_of(&w, MACHINE)
        .into_iter()
        .filter_map(|r| r.end)
        .collect();
    assert_eq!(ends, [EndKind::GuestLost]);
    assert!(w.m.registrations_to_remove().is_empty());
    w.assert_nothing_left();
}

#[tokio::test]
async fn a_machine_that_did_not_start_gives_its_slot_back_to_the_caller() {
    let w = World::new(1);
    let run = run(json!({}));
    let Admission::Start(ticket) = test_run::admit(&*w.m, &run, None) else {
        panic!("no slot");
    };
    w.parts.vms.fail_start(1);
    assert!(w.m.start_test_run(ticket, &run).is_err());
    assert_eq!(w.m.test_run_on(), None);
    assert!(w.parts.store.load().is_empty(), "a record of no machine is left");
    w.m.release(ticket);
    assert_eq!(w.slots(), 0);
    // The console sends the command again: it starts this time.
    assert_eq!(started(&w, &run), MACHINE);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_the_console_no_longer_asks_for_gives_its_place_back() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let mut w = World::new(1);
    let ci = w.running().await;
    let run = run(json!({"waitS": 3600}));
    let Admission::Waits(ticket) = test_run::admit(&*w.m, &run, None) else {
        panic!("no place in line");
    };
    let wanted = Arc::new(AtomicBool::new(true));
    let (m, still) = (w.m.clone(), wanted.clone());
    let waits = tokio::spawn(async move {
        test_run::wait_turn(&*m, ticket, Duration::from_secs(3600), move || {
            still.load(Ordering::SeqCst)
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(w.m.snapshot().waiting, 1);
    wanted.store(false, Ordering::SeqCst);
    let began = Instant::now();
    assert_eq!(waits.await.expect("the wait"), Waited::Withdrawn);
    assert!(began.elapsed() < Duration::from_secs(2), "it went on waiting");
    assert_eq!(w.m.snapshot().waiting, 0, "its place in line was left");

    w.parts.vms.end(job_name(&ci), End::Ok);
    w.github(EventKind::Completed, &ci, "succeeded");
    pass(&w.m, 3);
    assert_eq!(w.slots(), 0, "the room went to the run nobody wants");
    assert_eq!(w.parts.vms.started().len(), 1);
}

/// A workstation's settings: room for `machines` cf-2g machines while its person is away, `in_use` while they are
/// there (0: none at all).
fn workstation(machines: u32, in_use: u32) -> cf_budget::Settings {
    let mut settings = cf_daemon::testing::settings_for(machines);
    settings.mode = cf_budget::Mode::Workstation;
    settings.in_use.cap_bytes = u64::from(in_use) * Size::G2.vm_bytes();
    settings
}

#[tokio::test]
async fn a_waiting_run_does_not_start_while_someone_is_at_the_mac() {
    let mut w = World::on(workstation(1, 0));
    pass(&w.m, 15 * 60);
    let ci = w.running().await;
    let Admission::Waits(ticket) = test_run::admit(&*w.m, &run(json!({"waitS": 3600})), None) else {
        panic!("no place in line");
    };
    // The person comes back while the CI job ends: a job GitHub assigned would start; a test run waits for them.
    w.m.with_host(|host| host.touch());
    w.parts.vms.end(job_name(&ci), End::Ok);
    w.github(EventKind::Completed, &ci, "succeeded");
    pass(&w.m, 2);
    assert!(!w.m.ready(ticket), "the test run started with someone at the Mac");
    assert_eq!(w.slots(), 0);
    // They leave: it starts.
    pass(&w.m, 16 * 60);
    assert!(w.m.ready(ticket), "the test run never started once they left");
}

#[tokio::test]
async fn a_run_takes_room_only_under_the_limits_of_a_mac_in_use() {
    // Two machines while away, one while someone is there, who is there now: a CI job GitHub assigned runs.
    let mut w = World::on(workstation(2, 1));
    w.running().await;
    let admission = test_run::admit(&*w.m, &run(json!({"waitS": 0})), None);
    assert!(
        matches!(&admission, Admission::Busy(why) if why.starts_with("no room now")),
        "{admission:?}"
    );
    assert_eq!(w.slots(), 1);
}

#[tokio::test]
async fn a_docker_run_past_the_macs_docker_jobs_is_busy() {
    let w = World::new(3);
    let held = w.m.reserve_docker(Size::G2).expect("a Docker job's promise");
    let admission = test_run::admit(&*w.m, &run(json!({"docker": true})), None);
    assert_eq!(admission, Admission::Busy("docker-full".into()));
    assert_eq!(w.slots(), 1);
    // One without Docker is not held to it.
    assert!(matches!(
        test_run::admit(&*w.m, &run(json!({})), None),
        Admission::Start(_)
    ));
    w.m.release(held);
}

#[tokio::test]
async fn a_docker_run_holds_its_docker_room_while_it_waits() {
    let mut w = World::new(1);
    w.running().await;
    let Admission::Waits(ticket) = test_run::admit(&*w.m, &run(json!({"docker": true, "waitS": 60})), None)
    else {
        panic!("no place in line");
    };
    assert_eq!(
        w.m.docker_room(),
        0,
        "a waiting Docker run left its Docker room to others"
    );
    assert_eq!(
        w.m.reserve_docker(Size::G2).map_err(|refusal| refusal.0),
        Err("docker-full".to_string())
    );
    // Given up, its room is everyone's again.
    w.m.withdraw(ticket);
    assert_eq!(w.m.docker_room(), 1);
}

#[test]
fn the_agents_own_time_ends_before_the_machines_deadline() {
    let long = run(json!({"timeoutS": 7200}));
    assert_eq!(long.timeout_within(Duration::from_secs(6 * 3600)), Ok(7200));
    // A Runner whose jobs may run an hour: the suite gets what is left after the setup's and the end's time.
    assert_eq!(long.timeout_within(Duration::from_secs(3600)), Ok(3600 - 900));
    assert_eq!(long.timeout_within(Duration::from_secs(900 + 60)), Ok(60));
    assert!(long.timeout_within(Duration::from_secs(900 + 59)).is_err());
    let env = long.env(Duration::from_secs(3600));
    assert_eq!(env[2].0, "CF_RUN_TIMEOUT_S");
    assert_eq!(env[2].1.expose(), "2700");
    assert_eq!(
        long.deadline(Duration::from_secs(3600)),
        Duration::from_secs(3600)
    );
}

#[tokio::test]
async fn a_slot_given_to_a_paused_or_holding_host_starts_no_machine() {
    let w = World::new(2);
    let run = run(json!({}));
    for hold in [
        |m: &cf_daemon::testing::TestMachines, on| m.pause(on),
        |m: &cf_daemon::testing::TestMachines, on| m.hold_for_restart(on),
    ] {
        let Admission::Start(ticket) = test_run::admit(&*w.m, &run, None) else {
            panic!("no slot");
        };
        hold(&w.m, true);
        assert!(
            w.m.start_test_run(ticket, &run).is_err(),
            "a machine started on a held host"
        );
        w.m.release(ticket);
        hold(&w.m, false);
        assert_eq!(w.slots(), 0);
        assert!(w.parts.vms.started().is_empty());
    }
}

#[tokio::test]
async fn the_token_shows_in_no_debug_print_of_the_machines_request() {
    let w = World::new(1);
    let run = run(json!({}));
    started(&w, &run);
    let request = w.parts.vms.started().pop().expect("a machine");
    let shown = format!("{request:?}");
    assert!(!shown.contains(run.token.expose().as_str()), "{shown}");
    let message = cf_guest_proto::Job {
        name: request.job.clone(),
        host: request.host.clone(),
        workload: request.workload.clone(),
        limits: request.limits(),
        env: request.env.clone(),
        oom_grace_ms: 0,
        blocked: Vec::new(),
        docker: false,
        trusted: None,
    };
    assert!(!format!("{message:?}").contains(run.token.expose().as_str()));
    // On the wire it is there, as before: the vm-host and the guest read it.
    let wire = serde_json::to_string(&request).expect("json");
    assert!(
        wire.contains(&format!(r#"["CF_RUN_TOKEN","{}"]"#, run.token.expose())),
        "{wire}"
    );
}

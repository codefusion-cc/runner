//! The service's scenarios: the whole loop with a real machine, and the installed program's own commands.
//!
//! GitHub and the console are the repository's fakes, on this Mac. `service` builds the service the way the
//! program does (`cf_daemon::host_service`), with a shell script in the machine in place of the Actions runner, and
//! takes three jobs through it: one to its end, one across a Runner that dies and starts again, one into an
//! uninstall. `commands` runs the program itself: `enroll`, `check --github`, and `daemon` until a SIGTERM. Nothing
//! is installed: no launchd agent, no file outside the suite's folders.

use std::os::unix::fs::PermissionsExt as _;
use std::sync::Arc;

use cf_budget::{Limits, Mode};
use cf_daemon::names::ServePolicy;
use cf_daemon::{HostMachines, HostService, HostSetup, This, host_service, records_dir};
use cf_fake_console::FakeConsole;
use cf_fake_github::harness::REPO;
use cf_fake_github::{FakeGithub, Options, test_key};
use cf_ghapp::{Http, RetryPolicy, SystemClock};
use cf_guest_proto::Workload;
use cf_scaleset::ShutdownTrigger;
use cf_secret::Secret;
use tokio::runtime::Runtime;

use super::*;

const MACHINE: &str = "it-mac";
const CODE: &str = "one-time-code";
const WORKFLOW: &str = "on: push\njobs:\n  build:\n    runs-on: [self-hosted, cf-2g]\n    steps: []\n";
const LABELS: [&str; 2] = ["self-hosted", "cf-2g"];
/// The job in place of the Actions runner: it is there for twenty seconds.
const STAND_IN: &str = "echo job-started; sleep 20; echo job-done";
/// What a `cf-2g` machine and the Mac's floor need before the suite starts one beside whatever else runs here.
const NEEDED: u64 = 4608 << 20;
/// What must be available beyond the machine and the floor before a job is queued.
const SPARE: u64 = 256 << 20;

fn runtime() -> Fallible<Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}

/// GitHub and the console, for as long as a scenario runs.
struct World {
    runtime: Runtime,
    github: FakeGithub,
    console: FakeConsole,
}

impl World {
    fn start() -> Fallible<World> {
        let runtime = runtime()?;
        let (github, console) = runtime.block_on(async {
            let github = FakeGithub::start(Options {
                poll_timeout: Duration::from_secs(5),
                auto_assign: true,
                ..Options::default()
            })
            .await;
            (github, FakeConsole::start().await)
        });
        github.add_repo(REPO, true);
        github.set_workflow(REPO, "ci.yml", WORKFLOW);
        console.add_code(CODE);
        Ok(World {
            runtime,
            github,
            console,
        })
    }

    /// The job events the console took with this id.
    fn events(&self, id: &str) -> Vec<Value> {
        self.console
            .job_events()
            .into_iter()
            .filter(|event| event["id"] == json!(id))
            .collect()
    }
}

/// One Runner: what a process of the program holds. Dropping its runtime is the process dying.
struct Process {
    runtime: Runtime,
    machines: Arc<HostMachines>,
    service: Arc<HostService>,
    stop: ShutdownTrigger,
    run: tokio::task::JoinHandle<()>,
}

fn start_runner(bench: &Bench, world: &World, token: &Secret<String>) -> Fallible<Process> {
    let runtime = runtime()?;
    let (machines, service) = {
        let _inside = runtime.enter();
        host_service::<This>(HostSetup {
            machine: MACHINE.into(),
            version: "0.0.0-it".into(),
            state_dir: bench.state.clone(),
            program: bench.binary.clone(),
            app: cf_fake_github::harness::app(
                &world.github,
                Http::new(Duration::from_secs(30), RetryPolicy::default()),
                Arc::new(SystemClock),
            ),
            image: bench.image.clone(),
            deadline: Duration::from_secs(300),
            mode: Mode::Dedicated,
            limits: Limits::default(),
            policy: ServePolicy::default(),
            console: Some((world.console.url(), Secret::new(token.expose().clone()))),
            stand_in: Some(Workload::Command {
                argv: vec!["/bin/bash".into(), "-c".into(), STAND_IN.into()],
            }),
            updates: None,
            settings: None,
        })?
    };
    let service = Arc::new(service);
    let (stop, stopped) = cf_scaleset::shutdown();
    let running = service.clone();
    let run = runtime.spawn(async move { running.run(stopped).await });
    Ok(Process {
        runtime,
        machines,
        service,
        stop,
        run,
    })
}

/// Waits until the Mac takes a job: the budget watches it for a calm minute first, and a machine needs its
/// memory. The budget's own reading is asked, for a little more than the job's machine takes, so that the room
/// is still there when the job arrives. On a Mac that other work keeps busy it never comes, which is the budget's
/// rule and no failure of the service: the answer then says what kept it, for a `SKIPPED` line.
fn room(process: &Process) -> Result<(), String> {
    let settings = process.machines.settings();
    // What kept the Mac closed while it was waited for, in the budget's own words.
    let mut kept = BTreeSet::new();
    let mut available = 0;
    let fits = || {
        let snapshot = process.machines.snapshot();
        let needed = settings.host_bytes(cf_budget::Size::G2) + snapshot.budget.limits.floor_bytes + SPARE;
        available = snapshot
            .budget
            .reading
            .map_or(0, |reading| reading.available_bytes);
        if !snapshot.open {
            kept.insert(snapshot.reason);
        } else if available < needed {
            kept.insert("short of memory".to_string());
        }
        snapshot.open && available >= needed
    };
    if within(Duration::from_secs(240), fits) {
        return Ok(());
    }
    Err(format!(
        "this Mac had no room for a machine in 240 s ({}; {} available at the end): other work on it presses on its memory, and the budget starts no machine beside that",
        kept.into_iter().collect::<Vec<_>>().join(", "),
        mb(available),
    ))
}

/// The scenario's lines so far and why it went no further.
fn skipped_from_here(mut lines: Vec<String>, why: &str) -> Vec<String> {
    lines.push(format!("SKIPPED from here: {why}"));
    lines
}

/// A job in GitHub's queue gets a machine and GitHub hands it over. Says the Actions runner's name, the machine's
/// process and how long the machine took from the job's arrival.
fn job_running(world: &World, standing: &BTreeSet<u32>) -> Fallible<(String, u32, Duration)> {
    let before = world.github.runners();
    let began = Instant::now();
    world.github.queue_job(REPO, &LABELS, "build");
    let mut runner = String::new();
    expect(
        within(Duration::from_secs(120), || {
            runner = world
                .github
                .runners()
                .into_iter()
                .find(|name| !before.contains(name))
                .unwrap_or_default();
            !runner.is_empty()
        }),
        "no Actions runner was registered for the job in 120 s",
    )?;
    world.github.start_job(&runner);
    let mut machine = 0;
    expect(
        within(Duration::from_secs(90), || {
            machine = machines().difference(standing).next().copied().unwrap_or(0);
            machine != 0
        }),
        format!("no machine was started for {runner} in 90 s"),
    )?;
    let took = began.elapsed();
    let id = format!("{runner}/started");
    expect(
        within(Duration::from_secs(90), || world.events(&id).len() == 1),
        format!("the console heard no start of {runner} in 90 s"),
    )?;
    Ok((runner, machine, took))
}

/// The one report of this Actions runner's end at the console.
fn ended_once(world: &World, runner: &str) -> Fallible<Value> {
    let id = format!("{runner}/completed");
    expect(
        within(Duration::from_secs(90), || !world.events(&id).is_empty()),
        format!("the console heard no end of {runner} in 90 s"),
    )?;
    // A second report of the same end would come with the next report to the console.
    sleep(Duration::from_secs(3));
    let events = world.events(&id);
    expect(
        events.len() == 1 && world.events(&format!("{runner}/started")).len() == 1,
        format!("{runner} was reported {} times", events.len()),
    )?;
    Ok(events[0].clone())
}

fn records(bench: &Bench) -> usize {
    fs::read_dir(records_dir(&bench.state))
        .into_iter()
        .flatten()
        .flatten()
        .count()
}

/// Nothing is left of any job: no machine, no registration, no slot, no record.
fn nothing_left(
    bench: &Bench,
    world: &World,
    machines_of: &HostMachines,
    standing: &BTreeSet<u32>,
) -> Fallible<()> {
    let clean = || {
        let snapshot = machines_of.snapshot();
        world.github.runners().is_empty()
            && machines().is_subset(standing)
            && snapshot.machines.is_empty()
            && snapshot.budget.slots.is_empty()
            && records(bench) == 0
    };
    expect(
        within(Duration::from_secs(60), clean),
        format!(
            "left after the job: registrations {:?}, machines {:?}, slots {}, records {}",
            world.github.runners(),
            machines().difference(standing).collect::<Vec<_>>(),
            machines_of.snapshot().budget.slots.len(),
            records(bench)
        ),
    )
}

fn end_of(event: &Value) -> String {
    format!(
        "end `{}`, result `{}`, {} MB, booted in {} ms",
        event["end"].as_str().unwrap_or("none"),
        event["result"].as_str().unwrap_or("none"),
        event["machine"]["memoryBytes"].as_u64().unwrap_or(0) / MB,
        event["machine"]["bootMs"].as_u64().unwrap_or(0)
    )
}

/// Three jobs through the service with real machines: to its end, across a Runner that dies, into an uninstall.
pub fn service(bench: &Bench) -> Fallible<Vec<String>> {
    if available() < NEEDED {
        return Ok(vec![format!(
            "SKIPPED: {} of memory are available on this Mac and a cf-2g machine beside its floor needs {}",
            mb(available()),
            mb(NEEDED)
        )]);
    }
    let _ = fs::remove_dir_all(records_dir(&bench.state));
    let _ = fs::remove_file(bench.state.join("outbox.json"));
    // What the service logs, for whoever reads why a step failed.
    let log = bench.work.join("service.log");
    let _ = fs::remove_file(&log);
    let _ = tracing::subscriber::set_global_default(cf_daemon::Lines::new(&log, cf_daemon::ROLL_AT));
    let standing = machines();
    let world = World::start()?;
    let token = world
        .runtime
        .block_on(cf_console_client::enroll(
            &Http::new(Duration::from_secs(5), RetryPolicy::default()),
            &world.console.url(),
            &cf_console_client::Enroll {
                code: Secret::new(CODE.to_string()),
                machine: MACHINE.into(),
                hostname: "the-test-mac".into(),
                model: "Mac".into(),
                version: "0.0.0-it".into(),
                cpus: 8,
                memory_bytes: 16 << 30,
                os: cf_console_client::Os::this(),
            },
        ))?
        .token;
    let mut lines = Vec::new();

    // One job, from GitHub's queue to the console's page.
    let first = start_runner(bench, &world, &token)?;
    if let Err(why) = room(&first) {
        first.service.kill();
        return Ok(skipped_from_here(lines, &why));
    }
    let (runner, machine, took) = job_running(&world, &standing)?;
    expect(
        within(Duration::from_secs(90), || !alive(machine)),
        "the machine outlived its job by a minute",
    )?;
    world.github.complete_job(&runner, "succeeded");
    let event = ended_once(&world, &runner)?;
    expect(
        event["end"] == json!("success") && event["result"] == json!("succeeded"),
        format!("the job that succeeded was reported as {}", end_of(&event)),
    )?;
    expect(
        event["machine"]["memoryBytes"] == json!(2u64 << 30)
            && event["machine"]["bootMs"].as_u64().unwrap_or(0) > 0,
        format!("the machine of the job was reported as {}", event["machine"]),
    )?;
    nothing_left(bench, &world, &first.machines, &standing)?;
    lines.push(format!(
        "a job from the queue to the console: its machine stood {:.1} s after the job arrived; {}; nothing left",
        took.as_secs_f32(),
        end_of(&event)
    ));

    // The Runner dies under a job and the one that starts finds the machine.
    if let Err(why) = room(&first) {
        first.service.kill();
        return Ok(skipped_from_here(lines, &why));
    }
    let (runner, machine, _) = job_running(&world, &standing)?;
    first.service.kill();
    let Process {
        runtime, machines, ..
    } = first;
    runtime.shutdown_timeout(Duration::from_secs(3));
    drop(machines);
    sleep(Duration::from_secs(2));
    expect(alive(machine), "the machine died with the Runner")?;
    let second = start_runner(bench, &world, &token)?;
    expect(
        within(Duration::from_secs(30), || {
            second
                .machines
                .snapshot()
                .machines
                .iter()
                .any(|m| m.runner == runner)
        }),
        "the Runner that started did not find the machine of before",
    )?;
    // GitHub lets a session that nobody polls expire; the fake is told to. Until then the new Runner is refused.
    for set in world.github.scale_sets() {
        world.github.lose_session(set.id);
    }
    expect(
        within(Duration::from_secs(90), || !alive(machine)),
        "the machine outlived its job by a minute",
    )?;
    world.github.complete_job(&runner, "succeeded");
    let event = ended_once(&world, &runner)?;
    expect(
        event["end"] == json!("success"),
        format!(
            "the job that outlived a Runner was reported as {}",
            end_of(&event)
        ),
    )?;
    nothing_left(bench, &world, &second.machines, &standing)?;
    lines.push(format!(
        "a Runner that died under a job: the machine went on, the next Runner found it; {}; reported once; nothing left",
        end_of(&event)
    ));

    // An uninstall under a job: the machine is stopped, GitHub and the console hear of it, nothing stays.
    if let Err(why) = room(&second) {
        second.service.kill();
        return Ok(skipped_from_here(lines, &why));
    }
    let (runner, machine, _) = job_running(&world, &standing)?;
    second.stop.trigger();
    second.runtime.block_on(second.run)?;
    expect(
        alive(machine),
        "the machine was stopped by a Runner that only ended",
    )?;
    expect(
        world.github.scale_sets().iter().all(|set| !set.has_session),
        "a Runner that ended left a session open at GitHub",
    )?;
    let ticking = second.machines.clone();
    let ticks = second.runtime.spawn(async move {
        loop {
            let machines = ticking.clone();
            let _ = tokio::task::spawn_blocking(move || machines.tick()).await;
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
    let retiring = second.service.clone();
    let retire = second
        .runtime
        .spawn(async move { retiring.retire(Duration::from_secs(120), true).await });
    let began = Instant::now();
    expect(
        within(Duration::from_secs(60), || !alive(machine)),
        "the uninstall did not stop the machine in 60 s",
    )?;
    let stopped_after = began.elapsed();
    // GitHub lets a job go some time after its Actions runner fell silent; the fake is told to.
    world.github.complete_job(&runner, "failed");
    let left = second.runtime.block_on(retire)?;
    ticks.abort();
    expect(left.is_empty(), format!("the uninstall left: {left:?}"))?;
    let event = ended_once(&world, &runner)?;
    expect(
        world.github.scale_sets().is_empty() && world.github.runners().is_empty(),
        format!(
            "left at GitHub: scale sets {:?}, registrations {:?}",
            world.github.scale_sets().len(),
            world.github.runners()
        ),
    )?;
    expect(
        world.console.left() == [MACHINE.to_string()],
        "the console was not told the Mac is gone",
    )?;
    nothing_left(bench, &world, &second.machines, &standing)?;
    lines.push(format!(
        "an uninstall under a job: the machine was gone {:.1} s later; {}; no scale set, no registration, the console told",
        stopped_after.as_secs_f32(),
        end_of(&event)
    ));
    Ok(lines)
}

fn run_program(bench: &Bench, home: &Path, args: &[&str], input: &str) -> Fallible<(bool, String)> {
    let mut child = Command::new(&bench.binary)
        .args(args)
        .env("HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or("no standard input")?
        .write_all(input.as_bytes())?;
    let output = child.wait_with_output()?;
    Ok((
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    ))
}

/// The program's own commands against the fakes: `enroll`, `check --github`, `daemon` until a SIGTERM, and the
/// refusal to carry the cf-runners manager's name. No machine is started, and the home the program sees is a
/// folder of the suite.
pub fn commands(bench: &Bench) -> Fallible<Vec<String>> {
    // The configuration refuses a state folder on another volume, as launchd would; the suite's own is on one.
    let folder = std::env::temp_dir().join("cc.codefusion.runner.it-commands");
    let _ = fs::remove_dir_all(&folder);
    let (home, state, app) = (folder.join("home"), folder.join("state"), folder.join("app"));
    fs::create_dir_all(&home)?;
    fs::create_dir_all(&app)?;
    install(&bench.binary, &bench.release, &bench.image, &state)?;
    fs::write(
        app.join("app.json"),
        json!({ "client_id": "Iv23test" }).to_string(),
    )?;
    fs::write(app.join("app.pem"), &test_key().private_pem)?;
    fs::set_permissions(app.join("app.pem"), fs::Permissions::from_mode(0o600))?;
    let world = World::start()?;
    let config = folder.join("config.json");
    let configured = |machine: &str| {
        json!({
            "machine": machine,
            "state_dir": state,
            "app_dir": app,
            "github_api": world.github.url(),
            "image": bench.image,
            "console": world.console.url(),
            "use": "dedicated",
        })
        .to_string()
    };
    fs::write(&config, configured(MACHINE))?;
    let config_arg = config.display().to_string();
    let mut lines = Vec::new();

    let (ok, said) = run_program(bench, &home, &["check", "--config", &config_arg], "")?;
    expect(
        !ok && said.contains("MISSING") && said.contains("run `enroll` first"),
        format!("a check before the enrolment said: {said}"),
    )?;

    let (ok, said) = run_program(
        bench,
        &home,
        &["enroll", "--config", &config_arg],
        &format!("{CODE}\n"),
    )?;
    expect(ok, format!("enroll failed: {said}"))?;
    let token_file = state.join("console-token");
    let token = fs::read_to_string(&token_file)?;
    expect(
        world.console.tokens() == [token.clone()] && !said.contains(&token) && !said.contains(CODE),
        "the token the console gave is not the one kept, or it was printed",
    )?;
    expect(
        fs::metadata(&token_file)?.permissions().mode() & 0o777 == 0o600,
        "the token's file is readable by others",
    )?;
    let (ok, said) = run_program(
        bench,
        &home,
        &["enroll", "--config", &config_arg],
        &format!("{CODE}\n"),
    )?;
    expect(
        !ok && said.contains("is enrolled"),
        format!("a second enrolment said: {said}"),
    )?;
    lines.push(
        "enroll: the code went in on standard input, the token is kept for its owner alone, neither was printed; a second one is refused"
            .to_string(),
    );

    let (ok, said) = run_program(bench, &home, &["check", "--config", &config_arg, "--github"], "")?;
    expect(
        ok && said.contains("serves: org/app: [cf-2g, self-hosted] as cf-2g"),
        format!("check --github said: {said}"),
    )?;
    expect(
        world.github.scale_sets().is_empty() && world.github.runners().is_empty(),
        "a check made something at GitHub",
    )?;
    lines.extend(
        said.lines()
            .filter(|line| line.contains("side by side") || line.starts_with("serves:"))
            .map(|line| format!("check --github: {line}")),
    );

    let mut daemon = Command::new(&bench.binary)
        .args(["daemon", "--config", &config_arg])
        .env("HOME", &home)
        .stdout(Stdio::null())
        .stderr(fs::File::create(folder.join("daemon.stderr"))?)
        .spawn()?;
    let serving = within(Duration::from_secs(60), || {
        world.github.scale_sets().iter().any(|set| set.has_session)
            && world.console.reports().iter().any(|report| {
                report["status"]["machine"] == json!(MACHINE)
                    && report["status"]["scaleSets"][0]["repo"] == json!(REPO)
                    && report["status"]["host"]["bootTime"].is_string()
            })
    });
    if !serving {
        let _ = daemon.kill();
        let _ = daemon.wait();
        return Err(format!(
            "the daemon did not serve in 60 s: {}",
            tail(&fs::read_to_string(state.join("logs/runner.log")).unwrap_or_default())
        )
        .into());
    }
    signal(daemon.id(), "TERM");
    let began = Instant::now();
    let mut status = None;
    within(Duration::from_secs(30), || {
        status = daemon.try_wait().ok().flatten();
        status.is_some()
    });
    let Some(status) = status else {
        let _ = daemon.kill();
        let _ = daemon.wait();
        return Err("the daemon did not end in 30 s after a SIGTERM".into());
    };
    let log = fs::read_to_string(state.join("logs/runner.log"))?;
    expect(
        status.success() && log.contains("the Runner starts") && log.contains("the Runner ends"),
        format!("the daemon ended with {status}; its log: {}", tail(&log)),
    )?;
    expect(
        world.github.scale_sets().iter().all(|set| !set.has_session),
        "the daemon left its session open at GitHub",
    )?;
    expect(
        !log.contains(&token) && !log.contains("BEGIN RSA"),
        "the log holds a secret",
    )?;
    lines.push(format!(
        "daemon: a scale set with a session and a report with the Mac's boot time at the console; after a SIGTERM it closed its session and ended in {:.1} s",
        began.elapsed().as_secs_f32()
    ));

    // The cf-runners manager, as this home would have it, with the name the configuration asks for.
    fs::create_dir_all(home.join("Library/LaunchAgents"))?;
    fs::write(home.join("Library/LaunchAgents/cc.codefusion.runners.plist"), "")?;
    fs::create_dir_all(home.join(".config/codefusion-runners"))?;
    fs::write(
        home.join(".config/codefusion-runners/config.json"),
        json!({ "machine": MACHINE }).to_string(),
    )?;
    let calls = world.github.requests().len();
    let (ok, said) = run_program(bench, &home, &["daemon", "--config", &config_arg], "")?;
    expect(
        !ok && said.contains("the cf-runners manager on this Mac is called `it-mac` too"),
        format!("a daemon with the manager's name said: {said}"),
    )?;
    expect(
        world.github.requests().len() == calls,
        "a daemon with the manager's name called GitHub",
    )?;
    lines.push(
        "daemon: refused with the cf-runners manager's own name, before any call to GitHub".to_string(),
    );
    fs::remove_dir_all(&folder)?;
    Ok(lines)
}

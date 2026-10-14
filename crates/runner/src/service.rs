//! The Runner as a service of one Mac: `daemon` (what launchd runs), `enroll` at the console, `check` before an
//! install, `install` and `uninstall`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cf_budget::{Budget, Host as _, HostReading, Settings, Size, admit};
use cf_daemon::names::ServePolicy;
use cf_daemon::update::{STAYS_UP, SelfUpdate, UpdateFile};
use cf_daemon::{Lines, MacMachines, MacService, MacSetup, ROLL_AT, mac_service};
use cf_ghapp::{AppCredentials, GitHubApp, Http, RetryPolicy, SystemClock};
use cf_host_macos::{MacHost, host_name, model};
use cf_secret::Secret;

use crate::config::{Config, check_beside_old_manager};
use crate::hidden::{self, NoLine};
use crate::launchd::{AGENT_PAUSE, Agent, LABEL, launchctl, plist};
use crate::update::{self, MacUpdate, Started};

use crate::commands::VERSION;
/// The entitlement without which macOS starts no virtual machine for a program.
const VIRTUALIZATION: &str = "com.apple.security.virtualization";
/// How long GitHub and the console have to answer one call of a command.
const ANSWER: Duration = Duration::from_secs(30);
/// How long an uninstall waits for the machines to end and their registrations to go.
const RETIRE_WAIT: Duration = Duration::from_secs(120);

/// Set by a signal that asks the service to end.
static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn asked_to_stop(_signal: libc::c_int) {
    STOP.store(true, Ordering::Relaxed);
}

fn watch_signals() {
    let handler = asked_to_stop as extern "C" fn(libc::c_int) as libc::sighandler_t;
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: the handler only stores into an atomic, which a signal handler may do, and lives as long as
        // the program.
        unsafe {
            libc::signal(signal, handler);
        }
    }
}

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute())
        .ok_or_else(|| "HOME names no folder".into())
}

fn uid() -> u32 {
    // SAFETY: no arguments; it cannot fail.
    unsafe { libc::getuid() }
}

fn hostname() -> String {
    host_name().unwrap_or_default()
}

fn runtime(threads: usize) -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()
        .map_err(|error| format!("no runtime: {error}"))
}

fn app(config: &Config) -> Result<GitHubApp, String> {
    let credentials = AppCredentials::load(&config.app_dir)
        .map_err(|error| format!("the GitHub App in {}: {error}", config.app_dir.display()))?;
    Ok(GitHubApp::new(
        &credentials.client_id,
        credentials.key,
        &config.github_api,
        Http::new(ANSWER, RetryPolicy::default()),
        Arc::new(SystemClock),
    ))
}

/// This Mac's token for the console, when the configuration names one.
fn console(config: &Config) -> Result<Option<(String, Secret<String>)>, String> {
    let Some(address) = &config.console else {
        return Ok(None);
    };
    let path = config.console_token();
    let token = std::fs::read_to_string(&path)
        .map(|text| text.trim().to_string())
        .ok()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| format!("{}: no token for the console; run `enroll` first", path.display()))?;
    Ok(Some((address.clone(), Secret::new(token))))
}

fn installed_image(config: &Config) -> Result<PathBuf, String> {
    cf_image::store::Store::new(config.state_dir.join("images"))
        .rootfs(&config.image)
        .ok_or_else(|| {
            format!(
                "image {} is not installed in {}; run `image install`",
                config.image,
                config.state_dir.display()
            )
        })
}

fn service(
    config: &Config,
    program: &Path,
    updates: Option<Arc<dyn SelfUpdate>>,
) -> Result<(Arc<MacMachines>, MacService), String> {
    mac_service(MacSetup {
        machine: config.machine.clone(),
        version: VERSION.into(),
        state_dir: config.state_dir.clone(),
        program: program.to_path_buf(),
        app: app(config)?,
        image: config.image.clone(),
        deadline: config.deadline(),
        mode: config.mode(),
        policy: ServePolicy {
            repos: config.repos.clone(),
            public_repos: config.public_repos.clone(),
            docker_repos: config.docker_repos.clone(),
            docker_max_jobs: config.docker_max_jobs,
        },
        console: console(config)?,
        stand_in: None,
        updates,
    })
}

/// Asks Time Machine to skip a folder the Runner made: the images, scratch disks and machine files below it come from
/// a release or are made again, and a backup of gigabytes that change with every job helps nobody. Nothing here is
/// fatal: the answer is what to tell the person when the folder could not be marked, and `None` when it is.
pub(crate) fn keep_out_of_backups(folder: &Path) -> Option<String> {
    cf_host_macos::exclude_from_backup(folder)
        .err()
        .map(|error| format!("{} stays in Time Machine's backups: {error}", folder.display()))
}

/// The one Runner of a state folder: held for as long as the file it returns is. Two Runners over the same folder
/// would each end the machines the other started and remove what it wrote.
fn only_runner(config: &Config) -> Result<std::fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let folder = &config.state_dir;
    let said = |error: std::io::Error| format!("{}: {error}", folder.display());
    cf_secret::private_dir(folder).map_err(said)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(folder.join("runner.lock"))
        .map_err(said)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => Err(format!(
            "another Runner runs over {}; end it first",
            folder.display()
        )),
        Err(std::fs::TryLockError::Error(error)) => Err(said(error)),
    }
}

/// The configuration an uninstall works with: a Mac without a token it can use (it never enrolled, or its
/// enrolment was cut off) has nobody to tell, and must still be able to go.
fn for_uninstall(mut config: Config) -> Config {
    if console(&config).is_err() {
        config.console = None;
    }
    config
}

fn own_program() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("the Runner's own program: {error}"))
}

/// The state folder of the installed program `program` runs as: `<state>/bin/codefusion-runner`. `None` for a
/// program run from elsewhere, such as a build.
fn installed_state(program: &Path) -> Option<&Path> {
    let bin = program.parent()?;
    (bin.file_name()? == "bin").then_some(bin.parent()?)
}

/// What launchd runs: the service until it is asked to end. Machines that stand then go on, and the Runner that
/// starts next finds them.
pub fn daemon(config: &Path) -> Result<(), String> {
    let program = own_program()?;
    // First, before anything a new program could fail at: an update under way counts its starts, and puts the
    // program before back when it does not stay up.
    let started = match installed_state(&program) {
        Some(state) => update::guard(state, &program, VERSION)?,
        None => Started::Nothing,
    };
    let config = Config::load(config)?;
    check_beside_old_manager(&config, &home()?, &hostname())?;
    // A job is not taken from GitHub to fail for a missing image.
    installed_image(&config)?;
    let _only = only_runner(&config)?;
    tracing::subscriber::set_global_default(Lines::new(&config.log(), ROLL_AT))
        .map_err(|error| format!("the log: {error}"))?;
    if let Some(said) = keep_out_of_backups(&config.state_dir) {
        tracing::warn!("{said}");
    }
    // Only the installed program replaces itself: one run from elsewhere would replace another.
    let updates = (program == config.program())
        .then(|| {
            MacUpdate::of_this_program(
                &config.state_dir,
                &config.program(),
                &config.records(),
                config.build_folder.as_deref(),
                entitled,
            )
        })
        .flatten()
        .map(Arc::new);
    let runtime = runtime(2)?;
    let ran = runtime.block_on(async {
        let updates = updates.clone().map(|updates| updates as Arc<dyn SelfUpdate>);
        let (_machines, service) = service(&config, &program, updates)?;
        let (trigger, stop) = cf_scaleset::shutdown();
        watch_signals();
        tokio::spawn(async move {
            while !STOP.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            trigger.trigger();
        });
        tracing::info!(version = VERSION, machine = %config.machine, "the Runner starts");
        let stays_up = async {
            match started {
                Started::Nothing => {}
                Started::Failed { id, message } => {
                    tracing::warn!(%message, "the update did not take");
                    service.said(&id, false, &message);
                }
                Started::Trying(pending) => {
                    service.take_up(&pending);
                    tokio::time::sleep(STAYS_UP).await;
                    service.said(
                        &pending.id,
                        true,
                        &format!("now runs {} (was {})", pending.to, pending.from),
                    );
                    if let Err(error) = UpdateFile::in_state(&config.state_dir).remove() {
                        tracing::warn!(%error, "the update is done, and its file stays");
                    }
                }
            }
            std::future::pending::<()>().await;
        };
        tokio::select! {
            () = service.run(stop) => {}
            () = stays_up => {}
        }
        tracing::info!("the Runner ends; machines that stand go on and are found again");
        Ok(())
    });
    // An update's build is ended with the Runner (the console sends its command again to the one that starts
    // next), and a git call of one is not waited for.
    if let Some(updates) = &updates {
        updates.end_build();
    }
    runtime.shutdown_timeout(Duration::from_secs(5));
    if ran.is_ok()
        && let Some(state) = installed_state(&program)
        && let Err(error) = update::ended_in_order(state, VERSION)
    {
        tracing::warn!(%error, "the update under way still counts this start");
    }
    ran
}

/// The console's one-time code from standard input. A person at a terminal is told what is waited for, and what
/// they type is not shown; a pipe is asked nothing.
fn one_time_code(machine: &str) -> Result<Secret<String>, String> {
    let question =
        format!("The code the console's Add a Mac made for `{machine}` (it is not shown), then Enter: ");
    let line = match hidden::read_line(libc::STDIN_FILENO, &question, std::io::stderr()) {
        Ok(line) => line,
        Err(NoLine::Signal(signal)) => hidden::end_by(signal),
        Err(NoLine::NoCode) => return Err("the line on standard input is no code".into()),
        Err(NoLine::Io(error)) => return Err(format!("standard input: {error}")),
    };
    let code = line.trim();
    if code.is_empty() {
        return Err("no code on standard input".into());
    }
    Ok(Secret::new(code.to_string()))
}

/// Enrols this Mac at the console with the one-time code on standard input, and keeps the token the console
/// answers in the state folder, for this user alone. Neither is printed, and the code is not shown while it is
/// typed.
pub fn enroll(config: &Path) -> Result<(), String> {
    let config = Config::load(config)?;
    let address = config
        .console
        .clone()
        .ok_or("the configuration names no console")?;
    let path = config.console_token();
    if path.exists() {
        return Err(format!(
            "{} is there: this Mac is enrolled; `uninstall --leave` first",
            path.display()
        ));
    }
    let code = one_time_code(&config.machine)?;
    cf_secret::private_dir(&config.state_dir)
        .map_err(|error| format!("{}: {error}", config.state_dir.display()))?;
    if let Some(said) = keep_out_of_backups(&config.state_dir) {
        eprintln!("note: {said}");
    }
    let facts = MacHost::new(&config.state_dir)
        .and_then(|host| host.facts())
        .map_err(|error| format!("macOS: {error}"))?;
    let enrolled = runtime(1)?
        .block_on(cf_console_client::enroll(
            // The code is spent by the first request that reaches the console: a retry after a lost answer would
            // meet a token that is gone.
            &Http::new(
                ANSWER,
                RetryPolicy {
                    retries: 0,
                    ..RetryPolicy::default()
                },
            ),
            &address,
            &cf_console_client::Enroll {
                code,
                machine: config.machine.clone(),
                hostname: hostname(),
                model: model().unwrap_or_else(|| "Mac".into()),
                version: VERSION.into(),
                cpus: facts.cores,
                memory_bytes: facts.total_bytes,
            },
        ))
        .map_err(|error| format!("the console: {error}"))?;
    cf_secret::replace_private(&path, enrolled.token.expose().as_bytes())
        .map_err(|error| format!("{}: {error}", path.display()))?;
    println!(
        "ok: enrolled as {}; the token is in {}",
        enrolled.name,
        path.display()
    );
    Ok(())
}

/// Whether the program carries the entitlement macOS asks of a program that starts virtual machines.
pub(crate) fn entitled(program: &Path) -> Result<(), String> {
    let output = std::process::Command::new("/usr/bin/codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(program)
        .output()
        .map_err(|error| format!("codesign: {error}"))?;
    if output.status.success() && String::from_utf8_lossy(&output.stdout).contains(VIRTUALIZATION) {
        Ok(())
    } else {
        Err(format!(
            "{} is not signed with the {VIRTUALIZATION} entitlement; build it with `cargo xtask build`",
            program.display()
        ))
    }
}

/// What this Mac gives jobs under the configuration: the sizes it runs, with their CPUs, and how many of the
/// commonest fit side by side.
fn room(config: &Config) -> Result<String, String> {
    let facts = MacHost::new(Path::new("/"))
        .and_then(|host| host.facts())
        .map_err(|error| format!("macOS: {error}"))?;
    let settings = Settings::for_host(facts, config.mode());
    let sizes: Vec<String> = [Size::G2, Size::G4, Size::G6, Size::G8]
        .into_iter()
        .filter(|size| settings.can_run(*size).is_ok())
        .map(|size| format!("{} with {} CPUs", size.label(), settings.vcpus_of(size)))
        .collect();
    let side_by_side = settings.side_by_side(&settings.away, Size::G4);
    Ok(format!(
        "{} GB and {} cores, {}: {} GB for machines{}; {}; {} cf-4g side by side",
        facts.total_bytes >> 30,
        facts.cores,
        match config.mac_use {
            crate::config::Use::Dedicated => "nobody works here",
            crate::config::Use::Workstation => "someone's own Mac",
        },
        settings.away.cap_bytes as f64 / f64::from(1u32 << 30),
        match config.mac_use {
            crate::config::Use::Dedicated => "",
            crate::config::Use::Workstation => " while the person is away, none otherwise",
        },
        if sizes.is_empty() {
            "no size fits".to_string()
        } else {
            sizes.join(", ")
        },
        side_by_side,
    ))
}

/// The Mac's memory at this moment against the floor the budget keeps free, and the sizes a machine could have
/// right now. The budget's own numbers and arithmetic, so the line never disagrees with a Runner that runs; a
/// Runner still waits out its calm minute before it starts one.
fn memory_now(settings: &Settings, reading: &HostReading) -> String {
    // The budget itself says which limits are in force: with the person there, a workstation's are others.
    let limits = match Budget::new(settings.clone()) {
        Ok(mut budget) => {
            budget.observe(reading.at, Some(*reading));
            budget.status(reading.at).limits
        }
        Err(error) => return format!("the settings do not fit this Mac: {error}"),
    };
    let gib = |bytes: u64| bytes as f64 / cf_budget::GIB as f64;
    let fit: Vec<&str> = [Size::G2, Size::G4, Size::G6, Size::G8]
        .into_iter()
        .filter(|size| admit(settings, &limits, reading.available_bytes, &[], *size).is_ok())
        .map(Size::label)
        .collect();
    let floor = limits.floor_bytes;
    format!(
        "{:.2} GiB of memory available, {}; {}",
        gib(reading.available_bytes),
        if reading.available_bytes >= floor {
            format!(
                "{:.2} GiB above the floor of {:.2} GiB",
                gib(reading.available_bytes - floor),
                gib(floor)
            )
        } else {
            format!(
                "{:.2} GiB under the floor of {:.2} GiB",
                gib(floor - reading.available_bytes),
                gib(floor)
            )
        },
        if fit.is_empty() {
            "no machine fits beside what runs".to_string()
        } else {
            format!("a machine fits beside what runs: {}", fit.join(", "))
        },
    )
}

/// What the Mac reads at this moment, as a line of `check`. A look, not something an install needs.
fn now_line(config: &Config) -> String {
    let read = MacHost::new(Path::new("/")).and_then(|mut host| {
        let facts = host.facts()?;
        Ok((facts, host.read()?))
    });
    match read {
        Ok((facts, reading)) => memory_now(&Settings::for_host(facts, config.mode()), &reading),
        Err(error) => format!("macOS could not be read: {error}"),
    }
}

/// What an install needs, each as a line that says whether it is there. Nothing is changed.
fn needs(config: &Config, home: &Path, hostname: &str, program: &Path) -> Vec<Result<String, String>> {
    vec![
        check_beside_old_manager(config, home, hostname).map(|()| {
            match crate::config::old_manager_name(home, hostname) {
                Some(old) => format!(
                    "the name `{}` is not the cf-runners manager's (`{old}`), which is left as it is",
                    config.machine
                ),
                None => format!(
                    "the name `{}`; no cf-runners manager is installed for this user",
                    config.machine
                ),
            }
        }),
        AppCredentials::load(&config.app_dir)
            .map(|_| format!("the GitHub App's key in {}", config.app_dir.display()))
            .map_err(|error| format!("the GitHub App in {}: {error}", config.app_dir.display())),
        installed_image(config).map(|_| format!("image {}", config.image)),
        console(config).map(|console| match console {
            Some((address, _)) => format!("a token for the console at {address}"),
            None => "no console: job reports go to the log".to_string(),
        }),
        entitled(program).map(|()| format!("{} is signed for virtual machines", program.display())),
        room(config),
    ]
}

fn print(lines: &[Result<String, String>]) -> usize {
    for line in lines {
        match line {
            Ok(line) => println!("ok: {line}"),
            Err(line) => println!("MISSING: {line}"),
        }
    }
    lines.iter().filter(|line| line.is_err()).count()
}

/// Says what an install would find, and with `github` what GitHub shows this Mac: the repositories it would
/// serve and what the App may not read. It reads only: no scale set, no registration, no file.
pub fn check(config: &Path, github: bool) -> Result<(), String> {
    let config = Config::load(config)?;
    let program = own_program()?;
    let mut missing = print(&needs(&config, &home()?, &hostname(), &program));
    println!("now: {}", now_line(&config));
    if github {
        // The service is built on a folder of its own that is removed after: building it takes up what a Runner
        // that runs leaves in the real one (machines, records, job events), and a look must change none of that.
        let scratch = crate::vm::Scratch::new("check")?;
        let probe = Config {
            state_dir: scratch.path().to_path_buf(),
            console: None,
            ..config.clone()
        };
        let (served, problems) = runtime(1)?.block_on(async {
            let (_machines, service) = service(&probe, &program, None)?;
            Ok::<_, String>(service.survey().await)
        })?;
        for line in &served {
            println!("serves: {line}");
        }
        if served.is_empty() && problems.is_empty() {
            println!("serves: nothing; no workflow of a repository the App sees asks for this Mac");
        }
        for problem in &problems {
            println!("MISSING: {problem}");
        }
        missing += problems.len();
    }
    if missing == 0 {
        Ok(())
    } else {
        Err(format!("{missing} of the checks failed"))
    }
}

/// This person's agent, as the real launchd has it.
fn agent(home: &Path) -> Agent<'_> {
    Agent {
        home,
        uid: uid(),
        run: &launchctl,
        pause: AGENT_PAUSE,
    }
}

/// Installs this program as the person's launchd agent, or replaces the one installed before (the update). It is
/// refused while a machine stands: the Runner that starts would have to take over machines of another version.
pub fn install(config_path: &Path) -> Result<(), String> {
    let config = Config::load(config_path)?;
    let config_path =
        std::fs::canonicalize(config_path).map_err(|error| format!("{}: {error}", config_path.display()))?;
    let (home, program) = (home()?, own_program()?);
    if print(&needs(&config, &home, &hostname(), &program)) > 0 {
        return Err("not installed: the lines marked MISSING first".into());
    }
    let waits = "wait for its job to end, or `uninstall` to stop it";
    match crate::update::standing(&config.records()) {
        0 => {}
        n => return Err(format!("not installed: {n} machines stand; {waits}")),
    }
    let agent = agent(&home);
    let before = std::fs::read_to_string(agent.path()).ok();
    agent.stop()?;
    // A job may have come between the look and the stop.
    if let (n @ 1.., Some(before)) = (crate::update::standing(&config.records()), &before) {
        agent.start(before)?;
        return Err(format!(
            "not installed: {n} machines stand, the Runner of before runs again; {waits}"
        ));
    }
    crate::update::place(&program, &config.program(), entitled)?;
    if let Some(said) = keep_out_of_backups(&config.state_dir) {
        eprintln!("note: {said}");
    }
    agent.start(&plist(&config.program(), &config_path, &config.state_dir))?;
    println!(
        "ok: installed as {LABEL}; it runs {} and logs to {}",
        config.program().display(),
        config.log().display()
    );
    println!(
        "to undo: codefusion-runner uninstall --config {}",
        config_path.display()
    );
    Ok(())
}

/// Ends the agent, stops every machine, removes what this Mac has at GitHub and removes the agent and the
/// program's copy. `leave` also takes the Mac off the console; `purge` removes the state folder with the images
/// and the logs. What could not be removed is said, and is an error.
pub fn uninstall(config: &Path, leave: bool, purge: bool) -> Result<(), String> {
    let config = for_uninstall(Config::load(config)?);
    let home = home()?;
    let agent = agent(&home);
    agent.stop()?;
    // A Runner started by hand is not the agent's and was not ended with it.
    let _only = only_runner(&config)?;
    // The installed copy when it is there: a vm-host is known by the program it runs.
    let installed = config.program();
    let program = if installed.exists() {
        installed
    } else {
        own_program()?
    };
    let left = runtime(2)?.block_on(async {
        let (machines, service) = service(&config, &program, None).map_err(|error| {
            format!(
                "{error}; nothing was removed at GitHub, and a machine that stands ends with its job or at its \
                 deadline"
            )
        })?;
        let ticks = tokio::spawn(async move {
            loop {
                let machines = machines.clone();
                let _ = tokio::task::spawn_blocking(move || machines.tick()).await;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        let left = service.retire(RETIRE_WAIT, leave).await;
        ticks.abort();
        Ok::<_, String>(left)
    })?;
    agent.remove()?;
    for line in &left {
        println!("LEFT: {line}");
    }
    if !left.is_empty() {
        return Err(format!(
            "the agent is removed, {} things are not; run `uninstall` again",
            left.len()
        ));
    }
    let remove = |path: PathBuf| match std::fs::remove_file(&path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(format!("{}: {error}", path.display()))
        }
        _ => Ok(()),
    };
    remove(config.program())?;
    if leave {
        remove(config.console_token())?;
    }
    if purge {
        std::fs::remove_dir_all(&config.state_dir)
            .map_err(|error| format!("{}: {error}", config.state_dir.display()))?;
    }
    println!(
        "ok: uninstalled; {}",
        if purge {
            "the state folder is removed".to_string()
        } else {
            format!("{} is kept (`--purge` removes it)", config.state_dir.display())
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(state: &Path, app: &Path, console: bool) -> Config {
        Config::parse(&format!(
            r#"{{"machine": "mac-a-vm", "state_dir": "{}", "app_dir": "{}", "image": "2026.10.1", "use": "dedicated"{}}}"#,
            state.display(),
            app.display(),
            if console {
                r#", "console": "https://console.example""#
            } else {
                ""
            }
        ))
        .expect("a configuration")
    }

    /// A dedicated 16 GB Mac (floor 2 GiB) that reads this much available memory.
    fn memory_line(mode: cf_budget::Mode, available_bytes: u64, idle: cf_budget::Millis) -> String {
        let facts = cf_budget::HostFacts {
            total_bytes: 16 * cf_budget::GIB,
            cores: 8,
        };
        let mut host = cf_host_macos::FakeHost::new(facts);
        host.set(|reading| {
            reading.available_bytes = available_bytes;
            reading.idle = idle;
        });
        memory_now(&Settings::for_host(facts, mode), &host.read().expect("a reading"))
    }

    #[test]
    fn the_memory_line_of_a_check_says_what_is_there_now_against_the_floor_and_what_fits() {
        use cf_budget::{GIB, MIB, Millis, Mode};
        // The Air on 2026-10-11: 0.3 GiB above the floor, and `check` still read "2 cf-4g side by side".
        assert_eq!(
            memory_line(Mode::Dedicated, 2 * GIB + 307 * MIB, Millis::ZERO),
            "2.30 GiB of memory available, 0.30 GiB above the floor of 2.00 GiB; no machine fits beside what runs"
        );
        assert_eq!(
            memory_line(Mode::Dedicated, GIB + 512 * MIB, Millis::ZERO),
            "1.50 GiB of memory available, 0.50 GiB under the floor of 2.00 GiB; no machine fits beside what runs"
        );
        // A machine is counted as its size and the vm-host's own 192 MiB, on top of the floor: one byte less than
        // that is no room.
        let needs = |size: Size| 2 * GIB + size.vm_bytes() + 192 * MIB;
        assert!(
            memory_line(Mode::Dedicated, needs(Size::G2) - 1, Millis::ZERO)
                .ends_with("no machine fits beside what runs")
        );
        assert!(
            memory_line(Mode::Dedicated, needs(Size::G2), Millis::ZERO)
                .ends_with("a machine fits beside what runs: cf-2g")
        );
        assert!(
            memory_line(Mode::Dedicated, needs(Size::G4), Millis::ZERO)
                .ends_with("a machine fits beside what runs: cf-2g, cf-4g")
        );
        // Someone's own Mac keeps more free while the person is there, and all of it at the floor of a Mac nobody
        // is at once they are away.
        let there = memory_line(Mode::Workstation, 16 * GIB, Millis::ZERO);
        let away = memory_line(Mode::Workstation, 16 * GIB, Millis::mins(15));
        assert!(there.contains("above the floor of 4.00 GiB"), "{there}");
        assert!(away.contains("above the floor of 3.00 GiB"), "{away}");
    }

    #[test]
    fn a_check_names_everything_that_is_missing_and_changes_nothing() {
        let folder = tempfile::tempdir().expect("folder");
        let (state, app, home) = (
            folder.path().join("state"),
            folder.path().join("app"),
            folder.path().join("home"),
        );
        let config = config(&state, &app, true);
        let unsigned = folder.path().join("program");
        std::fs::write(&unsigned, "#!/bin/sh\n").expect("a program");
        let lines = needs(&config, &home, "Build-Mac-2.local", &unsigned);
        let missing: Vec<&String> = lines.iter().filter_map(|line| line.as_ref().err()).collect();
        assert_eq!(missing.len(), 4, "{lines:?}");
        assert!(missing[0].starts_with("the GitHub App in "), "{}", missing[0]);
        assert!(
            missing[1].starts_with("image 2026.10.1 is not installed"),
            "{}",
            missing[1]
        );
        assert!(missing[2].ends_with("run `enroll` first"), "{}", missing[2]);
        assert!(missing[3].contains("is not signed with the com.apple.security.virtualization"));
        assert!(lines[0].is_ok() && lines[5].is_ok(), "{lines:?}");
        assert!(!state.exists() && !app.exists(), "a check makes no folder");
    }

    #[test]
    fn a_token_file_that_is_empty_is_no_token() {
        let folder = tempfile::tempdir().expect("folder");
        let with = config(folder.path(), folder.path(), true);
        assert!(console(&with).is_err(), "no file");
        std::fs::write(with.console_token(), "\n").expect("a file");
        assert!(console(&with).is_err());
        std::fs::write(with.console_token(), " the-token\n").expect("a file");
        let (address, token) = console(&with).expect("a token").expect("a console");
        assert_eq!(address, "https://console.example");
        assert_eq!(token.expose(), "the-token");
        let none = config(folder.path(), folder.path(), false);
        assert!(console(&none).expect("no console is no error").is_none());
    }

    #[test]
    fn a_mac_without_a_token_it_can_use_is_uninstalled_without_the_console() {
        let folder = tempfile::tempdir().expect("folder");
        let with = || config(folder.path(), folder.path(), true);
        assert_eq!(for_uninstall(with()).console, None, "it never enrolled");
        // An enrolment that was cut off, or a token removed by hand.
        std::fs::write(with().console_token(), "\n").expect("a file");
        assert_eq!(for_uninstall(with()).console, None, "an empty file is no token");
        std::fs::write(with().console_token(), "the-token\n").expect("a file");
        assert_eq!(
            for_uninstall(with()).console.as_deref(),
            Some("https://console.example"),
            "an enrolled Mac tells the console"
        );
    }

    #[test]
    fn a_state_folder_has_one_runner_at_a_time() {
        let folder = tempfile::tempdir().expect("folder");
        let config = config(&folder.path().join("state"), folder.path(), false);
        let first = only_runner(&config).expect("the first Runner");
        let refused = only_runner(&config).expect_err("a second Runner over the same folder");
        assert!(
            refused.contains("another Runner") && refused.contains(&config.state_dir.display().to_string()),
            "{refused}"
        );
        drop(first);
        // A process another test starts at this moment holds a copy of the lock until it has become its program.
        let began = std::time::Instant::now();
        while only_runner(&config).is_err() && began.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        only_runner(&config).expect("the folder is free once its Runner is gone");
    }

    #[test]
    fn the_state_folder_is_marked_for_time_machine_once_and_a_failure_is_only_a_line_to_say() {
        let folder = tempfile::tempdir().expect("folder");
        let state = folder.path().join("state");
        std::fs::create_dir(&state).expect("folder");
        for _ in 0..2 {
            assert_eq!(keep_out_of_backups(&state), None);
        }
        let mark = std::process::Command::new("/usr/bin/xattr")
            .args(["-px", cf_host_macos::ATTRIBUTE])
            .arg(&state)
            .output()
            .expect("xattr runs");
        assert!(mark.status.success(), "the folder carries the mark");

        let gone = folder.path().join("gone");
        let said = keep_out_of_backups(&gone).expect("a folder that is not there cannot be marked");
        assert!(
            said.contains(&gone.display().to_string()) && said.contains("Time Machine"),
            "{said}"
        );
    }

    #[test]
    fn a_program_is_placed_only_when_it_is_signed_for_virtual_machines() {
        let folder = tempfile::tempdir().expect("folder");
        let unsigned = folder.path().join("program");
        std::fs::write(&unsigned, "#!/bin/sh\n").expect("a program");
        let at = folder.path().join("bin/codefusion-runner");
        std::fs::create_dir_all(at.parent().expect("a folder")).expect("folder");
        std::fs::write(&at, "the one before").expect("a program");
        assert!(crate::update::place(&unsigned, &at, entitled).is_err());
        assert_eq!(
            std::fs::read_to_string(&at).expect("the program"),
            "the one before",
            "the program that runs is not replaced by one that cannot start a machine"
        );
        let left: Vec<_> = std::fs::read_dir(at.parent().expect("a folder"))
            .expect("the folder")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(left, ["codefusion-runner"], "the copy that was refused is gone");
    }

    #[test]
    fn what_stands_is_counted_from_the_records_and_the_look_changes_nothing() {
        let folder = tempfile::tempdir().expect("folder");
        let config = config(folder.path(), folder.path(), false);
        assert_eq!(crate::update::standing(&config.records()), 0, "no folder yet");
        std::fs::create_dir_all(config.records()).expect("folder");
        assert_eq!(crate::update::standing(&config.records()), 0);
        std::fs::write(config.records().join("j1.json"), "{}").expect("a record");
        std::fs::write(config.records().join("j2.json"), "not a record").expect("a record");
        std::fs::write(config.records().join("j3.json.tmp"), "half written").expect("a file");
        assert_eq!(crate::update::standing(&config.records()), 2);
        assert!(config.records().join("j2.json").exists());
    }

    #[test]
    fn only_a_program_in_a_bin_folder_has_a_state_folder() {
        assert_eq!(
            installed_state(Path::new("/s/state/bin/codefusion-runner")),
            Some(Path::new("/s/state"))
        );
        assert_eq!(
            installed_state(Path::new("/s/target/release/codefusion-runner")),
            None
        );
        assert_eq!(installed_state(Path::new("/bin")), None);
        assert_eq!(installed_state(Path::new("codefusion-runner")), None);
    }
}

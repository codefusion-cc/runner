//! The Runner as a service of one Mac: `daemon` (what launchd runs), `enroll` at the console, `check` before an
//! install, `install` and `uninstall`.

use std::io::BufRead as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use cf_budget::{Host as _, Settings, Size};
use cf_daemon::names::ServePolicy;
use cf_daemon::{Lines, MacMachines, MacService, MacSetup, ROLL_AT, mac_service};
use cf_ghapp::{AppCredentials, GitHubApp, Http, RetryPolicy, SystemClock};
use cf_host_macos::{MacHost, host_name, model};
use cf_secret::Secret;

use crate::config::{Config, check_beside_old_manager};
use crate::launchd::{Agent, LABEL, launchctl, plist};

const VERSION: &str = env!("CARGO_PKG_VERSION");
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

fn service(config: &Config, program: &Path) -> Result<(Arc<MacMachines>, MacService), String> {
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
        },
        console: console(config)?,
        stand_in: None,
    })
}

fn own_program() -> Result<PathBuf, String> {
    std::env::current_exe().map_err(|error| format!("the Runner's own program: {error}"))
}

/// What launchd runs: the service until it is asked to end. Machines that stand then go on, and the Runner that
/// starts next finds them.
pub fn daemon(config: &Path) -> Result<(), String> {
    let config = Config::load(config)?;
    check_beside_old_manager(&config, &home()?, &hostname())?;
    // A job is not taken from GitHub to fail for a missing image.
    installed_image(&config)?;
    tracing::subscriber::set_global_default(Lines::new(&config.log(), ROLL_AT))
        .map_err(|error| format!("the log: {error}"))?;
    let program = own_program()?;
    runtime(2)?.block_on(async {
        let (_machines, service) = service(&config, &program)?;
        let (trigger, stop) = cf_scaleset::shutdown();
        watch_signals();
        tokio::spawn(async move {
            while !STOP.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            trigger.trigger();
        });
        tracing::info!(version = VERSION, machine = %config.machine, "the Runner starts");
        service.run(stop).await;
        tracing::info!("the Runner ends; machines that stand go on and are found again");
        Ok(())
    })
}

/// Enrols this Mac at the console with the one-time code on standard input, and keeps the token the console
/// answers in the state folder, for this user alone. Neither is printed.
pub fn enroll(config: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt as _;
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
    let mut code = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut code)
        .map_err(|error| format!("standard input: {error}"))?;
    let code = code.trim().to_string();
    if code.is_empty() {
        return Err("no code on standard input".into());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&config.state_dir)
        .map_err(|error| format!("{}: {error}", config.state_dir.display()))?;
    let facts = MacHost::new(&config.state_dir)
        .and_then(|host| host.facts())
        .map_err(|error| format!("macOS: {error}"))?;
    let enrolled = runtime(1)?
        .block_on(cf_console_client::enroll(
            &Http::new(ANSWER, RetryPolicy::default()),
            &address,
            &cf_console_client::Enroll {
                code: Secret::new(code),
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
fn entitled(program: &Path) -> Result<(), String> {
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
    if github {
        let (served, problems) = runtime(1)?.block_on(async {
            let (_machines, service) = service(&config, &program)?;
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

/// How many machines have a record. Counted from the files, so that a look from beside a Runner that runs
/// changes nothing; a record nobody can read counts, since its machine may stand.
fn standing(config: &Config) -> usize {
    std::fs::read_dir(config.records())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
        .count()
}

/// Puts a copy of the program where the agent runs it from, in one step: a program that runs is never written
/// into.
fn place(program: &Path, at: &Path) -> Result<(), String> {
    let folder = at.parent().ok_or("the program's folder has no parent")?;
    std::fs::create_dir_all(folder).map_err(|error| format!("{}: {error}", folder.display()))?;
    let fresh = at.with_extension("new");
    std::fs::copy(program, &fresh).map_err(|error| format!("{}: {error}", fresh.display()))?;
    entitled(&fresh)?;
    std::fs::rename(&fresh, at).map_err(|error| format!("{}: {error}", at.display()))
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
    match standing(&config) {
        0 => {}
        n => return Err(format!("not installed: {n} machines stand; {waits}")),
    }
    let agent = Agent {
        home: &home,
        uid: uid(),
        run: &launchctl,
        pause: Duration::from_millis(200),
    };
    let before = std::fs::read_to_string(agent.path()).ok();
    agent.stop()?;
    // A job may have come between the look and the stop.
    if let (n @ 1.., Some(before)) = (standing(&config), &before) {
        agent.start(before)?;
        return Err(format!(
            "not installed: {n} machines stand, the Runner of before runs again; {waits}"
        ));
    }
    place(&program, &config.program())?;
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
    let config = Config::load(config)?;
    let home = home()?;
    let agent = Agent {
        home: &home,
        uid: uid(),
        run: &launchctl,
        pause: Duration::from_millis(200),
    };
    agent.stop()?;
    // The installed copy when it is there: a vm-host is known by the program it runs.
    let program = Some(config.program())
        .filter(|program| program.exists())
        .map_or_else(own_program, Ok)?;
    let left = runtime(2)?.block_on(async {
        let (machines, service) = service(&config, &program).map_err(|error| {
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
    fn a_program_is_placed_only_when_it_is_signed_for_virtual_machines() {
        let folder = tempfile::tempdir().expect("folder");
        let unsigned = folder.path().join("program");
        std::fs::write(&unsigned, "#!/bin/sh\n").expect("a program");
        let at = folder.path().join("bin/codefusion-runner");
        std::fs::create_dir_all(at.parent().expect("a folder")).expect("folder");
        std::fs::write(&at, "the one before").expect("a program");
        assert!(place(&unsigned, &at).is_err());
        assert_eq!(
            std::fs::read_to_string(&at).expect("the program"),
            "the one before",
            "the program that runs is not replaced by one that cannot start a machine"
        );
    }

    #[test]
    fn what_stands_is_counted_from_the_records_and_the_look_changes_nothing() {
        let folder = tempfile::tempdir().expect("folder");
        let config = config(folder.path(), folder.path(), false);
        assert_eq!(standing(&config), 0, "no folder yet");
        std::fs::create_dir_all(config.records()).expect("folder");
        assert_eq!(standing(&config), 0);
        std::fs::write(config.records().join("j1.json"), "{}").expect("a record");
        std::fs::write(config.records().join("j2.json"), "not a record").expect("a record");
        std::fs::write(config.records().join("j3.json.tmp"), "half written").expect("a file");
        assert_eq!(standing(&config), 2);
        assert!(config.records().join("j2.json").exists());
    }
}

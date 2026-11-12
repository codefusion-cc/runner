use std::path::Path;

#[cfg(any(target_os = "macos", target_os = "linux"))]
use cf_daemon::{Platform as _, This};
use cf_image::store::Store;
use cf_image::{DirFiles, TRUSTED_PUBLIC_KEY, parse_public_key, verify};

/// The Runner's version with the commit the program was built at (`build.rs`), such as `0.1.0+446125c0a1b2`;
/// `unknown` for a commit nobody could name. One text for `version`, the log's first line and the console.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("CF_BUILT_AT"));

const USAGE: &str = "usage: codefusion-runner
  version
  check --config <file> [--github]      what an install needs and, with --github, what GitHub shows this host
  enroll --config <file>                enrol at the console with the one-time code on standard input
  install --config <file>               install or update the service (a launchd agent on macOS, a systemd
                                        user unit on Linux)
  uninstall --config <file> [--leave] [--purge]
                                        stop every machine and remove the service; --leave the console too,
                                        --purge the state folder
  daemon --config <file>                the service (what the service manager runs)
  pause --config <file> --hold <name> [--reason <text>] [--wait [--timeout <minutes>]]
                                        take no new job until the hold <name> is lifted (a-z, 0-9, -; 32 at
                                        most); --wait until no job runs, for 60 minutes or --timeout, and
                                        exit 75 with the hold still set when the time runs out
  resume --config <file> --hold <name>  lift the hold <name>; one not set is no error
  status --config <file> [--json]       the holds, whether the host takes jobs, and the machines that run
  image verify <folder> --version <v>
  image install <folder> --version <v> --state-dir <folder> [--engine <socket>]
                                        with --engine, a container-image release, whose root file
                                        system goes to that engine as codefusion-runner-guest:<v>
  vm --state-dir <folder> [--host-cpus <list>] [--engine <socket> --machine <name>]
                                        the vm-host of one job; on Linux --host-cpus are the CPUs its
                                        machine runs on (such as 4-7), by default the fastest; with
                                        --engine the job runs in a container of that engine
  vmm                                   the virtual machine monitor this build carries (Linux)
  boot-proof [--memory-mb N] [--timeout-s N, at most 3600] [--host-cpus <list>]
             [--engine <socket> --image <version>]
                                        with --engine, a container of the image <version> the engine
                                        holds in place of a machine";

/// The commands on the machine for its holds (`pause`, `resume`, `status`), which end with an exit code of their
/// own ([`crate::holds::TIMED_OUT`]); none for any other command.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn on_machine(args: &[String]) -> Option<std::process::ExitCode> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let [command, "--config", config, options @ ..] = words.as_slice() else {
        return None;
    };
    Some(match crate::holds::Asked::from_args(command, options)? {
        Ok(asked) => crate::holds::run(Path::new(config), asked),
        Err(message) => {
            eprintln!("codefusion-runner: {message}\n{USAGE}");
            std::process::ExitCode::FAILURE
        }
    })
}

pub fn run(args: &[String]) -> Result<(), String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["version"] | ["--version"] => {
            println!("codefusion-runner {VERSION}");
            Ok(())
        }
        ["image", "verify", folder, "--version", version] => verify_folder(folder, version),
        [
            "image",
            "install",
            folder,
            "--version",
            version,
            "--state-dir",
            state,
        ] => install(folder, version, state, None),
        [
            "image",
            "install",
            folder,
            "--version",
            version,
            "--state-dir",
            state,
            "--engine",
            socket,
        ] => install(folder, version, state, Some(socket)),
        ["vm", "--state-dir", state, options @ ..] => vm_host(state, options),
        [
            command @ ("daemon" | "enroll" | "check" | "install" | "uninstall"),
            "--config",
            config,
            options @ ..,
        ] => service(command, config, options),
        ["boot-proof", rest @ ..] => boot_proof(rest),
        #[cfg(target_os = "linux")]
        ["vmm"] => crate::linux::vmm(),
        _ => Err(USAGE.into()),
    }
}

/// Checks a downloaded release folder against the committed public key. The command keeps nothing between runs,
/// so it cannot say whether the release is older than one accepted before.
fn verify_folder(folder: &str, version: &str) -> Result<(), String> {
    let key = parse_public_key(TRUSTED_PUBLIC_KEY).map_err(|error| error.to_string())?;
    let verified =
        verify(&DirFiles::new(folder), &key, version, 0).map_err(|error| format!("refused: {error}"))?;
    println!("ok: image {} verified", verified.manifest.version);
    Ok(())
}

/// Installs a downloaded release folder into the state folder's images, where a job's machine takes its root
/// file system from; with an engine's socket, a container-image release, whose root file system the engine holds as
/// an image. The store there remembers the newest release it accepted and refuses an older one.
fn install(folder: &str, version: &str, state: &str, engine: Option<&str>) -> Result<(), String> {
    let key = parse_public_key(TRUSTED_PUBLIC_KEY).map_err(|error| error.to_string())?;
    let store = Store::new(Path::new(state).join("images"));
    let installed = match engine {
        None => store.install(Path::new(folder), version, &key),
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Some(socket) => {
            if !Path::new(socket).is_absolute() {
                return Err(format!(
                    "--engine {socket}: the engine's socket is an absolute path"
                ));
            }
            let mut images = cf_vmm_oci::image::Images::new(Path::new(socket));
            store.install_to(
                Path::new(folder),
                version,
                &key,
                cf_image::store::Target::Engine(&mut images),
            )
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        Some(_) => return Err("container mode runs on macOS and Linux only".into()),
    };
    let manifest = installed.map_err(|error| error.to_string())?;
    println!("ok: image {} installed", manifest.version);
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if let Some(said) = This::keep_state(Path::new(state)) {
        eprintln!("note: {said}");
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn service(command: &str, config: &str, options: &[&str]) -> Result<(), String> {
    let config = Path::new(config);
    let has = |option: &str| options.contains(&option);
    let only = |known: &[&str]| options.iter().all(|option| known.contains(option));
    match command {
        "daemon" if options.is_empty() => crate::service::daemon(config),
        "enroll" if options.is_empty() => crate::service::enroll(config),
        "check" if only(&["--github"]) => crate::service::check(config, has("--github")),
        "install" if options.is_empty() => crate::service::install(config),
        "uninstall" if only(&["--leave", "--purge"]) => {
            crate::service::uninstall(config, has("--leave"), has("--purge"))
        }
        _ => Err(USAGE.into()),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn service(_command: &str, _config: &str, _options: &[&str]) -> Result<(), String> {
    Err("the service runs on macOS and Linux only".into())
}

/// The options of `vm` and `boot-proof` that say where machines run, each at most once: `--host-cpus`, and
/// `--engine` with the one `with_engine` names beside it (`--machine` or `--image`). Any other is refused.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn host_options<'a>(
    options: &[&'a str],
    with_engine: &str,
    mut other: impl FnMut(&'a str, &'a str) -> Result<(), String>,
) -> Result<crate::vm::Host, String> {
    let (mut cpus, mut engine, mut beside) = (None, None, None);
    let mut iter = options.iter();
    while let Some(&option) = iter.next() {
        let &value = iter.next().ok_or(USAGE)?;
        let slot = match option {
            "--host-cpus" => &mut cpus,
            "--engine" => &mut engine,
            _ if option == with_engine => &mut beside,
            _ => {
                other(option, value)?;
                continue;
            }
        };
        if slot.replace(value).is_some() {
            return Err(format!("{option} is given twice"));
        }
    }
    let engine = match (engine, beside) {
        (None, None) => None,
        (Some(socket), Some(name)) if Path::new(socket).is_absolute() => {
            Some((std::path::PathBuf::from(socket), name.to_string()))
        }
        (Some(_), Some(_)) => return Err("--engine is the engine socket's absolute path".into()),
        _ => return Err(format!("--engine and {with_engine} go together")),
    };
    Ok(crate::vm::Host {
        cpus: cpus.map(String::from),
        engine,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn vm_host(state: &str, options: &[&str]) -> Result<(), String> {
    let host = host_options(options, "--machine", |_, _| Err(USAGE.into()))?;
    crate::vm::host(state, &host)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn vm_host(_state: &str, _options: &[&str]) -> Result<(), String> {
    Err("a vm-host runs on macOS and Linux only".into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn boot_proof(options: &[&str]) -> Result<(), String> {
    let mut memory_mb = 1024u64;
    let mut timeout = 60u64;
    let host = host_options(options, "--image", |option, value| {
        let number = || value.parse::<u64>().map_err(|_| USAGE.to_string());
        match option {
            "--memory-mb" => memory_mb = number()?,
            "--timeout-s" => timeout = number()?,
            _ => return Err(USAGE.into()),
        }
        Ok(())
    })?;
    // A proof is over in seconds; an hour is already no proof, and a time no clock can add is none either.
    if memory_mb == 0 || timeout == 0 || timeout > 3600 {
        return Err(USAGE.into());
    }
    let memory_bytes = memory_mb
        .checked_mul(1024 * 1024)
        .ok_or("--memory-mb is too large")?;
    crate::vm::boot_proof(memory_bytes, std::time::Duration::from_secs(timeout), &host)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_proof(_options: &[&str]) -> Result<(), String> {
    Err("the boot proof runs on macOS and Linux only".into())
}

use std::path::Path;

use cf_image::store::Store;
use cf_image::{DirFiles, TRUSTED_PUBLIC_KEY, parse_public_key, verify};

/// The Runner's version with the commit the program was built at (`build.rs`), such as `0.1.0+446125c0a1b2`;
/// `unknown` for a commit nobody could name. One text for `version`, the log's first line and the console.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("CF_BUILT_AT"));

const USAGE: &str = "usage: codefusion-runner
  version
  check --config <file> [--github]      what an install needs and, with --github, what GitHub shows this Mac
  enroll --config <file>                enrol at the console with the one-time code on standard input
  install --config <file>               install or update the launchd agent
  uninstall --config <file> [--leave] [--purge]
                                        stop every machine and remove the agent; --leave the console too,
                                        --purge the state folder
  daemon --config <file>                the service (what the agent runs)
  image verify <folder> --version <v>
  image install <folder> --version <v> --state-dir <folder>
  vm --state-dir <folder> [--host-cpus <list>]
                                        the vm-host of one job; on Linux --host-cpus are the CPUs its
                                        machine runs on (such as 4-7), by default the fastest
  boot-proof [--memory-mb N] [--timeout-s N, at most 3600] [--host-cpus <list>]";

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
        ] => install(folder, version, state),
        ["vm", "--state-dir", state] => vm_host(state, None),
        ["vm", "--state-dir", state, "--host-cpus", cpus] => vm_host(state, Some(cpus)),
        [
            command @ ("daemon" | "enroll" | "check" | "install" | "uninstall"),
            "--config",
            config,
            options @ ..,
        ] => service(command, config, options),
        ["boot-proof", rest @ ..] => boot_proof(rest),
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
/// file system from. The store there remembers the newest release it accepted and refuses an older one.
fn install(folder: &str, version: &str, state: &str) -> Result<(), String> {
    let key = parse_public_key(TRUSTED_PUBLIC_KEY).map_err(|error| error.to_string())?;
    let manifest = Store::new(Path::new(state).join("images"))
        .install(Path::new(folder), version, &key)
        .map_err(|error| error.to_string())?;
    println!("ok: image {} installed", manifest.version);
    #[cfg(target_os = "macos")]
    if let Some(said) = crate::service::keep_out_of_backups(Path::new(state)) {
        eprintln!("note: {said}");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
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

#[cfg(not(target_os = "macos"))]
fn service(_command: &str, _config: &str, _options: &[&str]) -> Result<(), String> {
    Err("the service runs on macOS only".into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn vm_host(state: &str, cpus: Option<&str>) -> Result<(), String> {
    let host = crate::vm::Host {
        cpus: cpus.map(String::from),
    };
    crate::vm::host(state, &host)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn vm_host(_state: &str, _cpus: Option<&str>) -> Result<(), String> {
    Err("a vm-host runs on macOS and Linux only".into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn boot_proof(options: &[&str]) -> Result<(), String> {
    let mut memory_mb = 1024u64;
    let mut timeout = 60u64;
    let mut cpus = None;
    let mut iter = options.iter();
    while let Some(option) = iter.next() {
        let value = iter.next().ok_or(USAGE)?;
        let number = || value.parse::<u64>().map_err(|_| USAGE.to_string());
        match *option {
            "--memory-mb" => memory_mb = number()?,
            "--timeout-s" => timeout = number()?,
            "--host-cpus" => cpus = Some(*value),
            _ => return Err(USAGE.into()),
        }
    }
    // A proof is over in seconds; an hour is already no proof, and a time no clock can add is none either.
    if memory_mb == 0 || timeout == 0 || timeout > 3600 {
        return Err(USAGE.into());
    }
    let memory_bytes = memory_mb
        .checked_mul(1024 * 1024)
        .ok_or("--memory-mb is too large")?;
    crate::vm::boot_proof(
        memory_bytes,
        std::time::Duration::from_secs(timeout),
        &crate::vm::Host {
            cpus: cpus.map(String::from),
        },
    )
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn boot_proof(_options: &[&str]) -> Result<(), String> {
    Err("the boot proof runs on macOS and Linux only".into())
}

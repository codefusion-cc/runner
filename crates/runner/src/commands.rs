use std::path::Path;

use cf_image::store::Store;
use cf_image::{DirFiles, TRUSTED_PUBLIC_KEY, parse_public_key, verify};

const USAGE: &str = "usage: codefusion-runner version | image verify <folder> --version <v> | image install <folder> --version <v> --state-dir <folder> | vm --state-dir <folder> | boot-proof [--memory-mb N] [--timeout-s N]";

pub fn run(args: &[String]) -> Result<(), String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["version"] | ["--version"] => {
            println!("codefusion-runner {}", env!("CARGO_PKG_VERSION"));
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
        ["vm", "--state-dir", state] => vm_host(state),
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
    Ok(())
}

#[cfg(target_os = "macos")]
fn vm_host(state: &str) -> Result<(), String> {
    crate::vm::host(state)
}

#[cfg(not(target_os = "macos"))]
fn vm_host(_state: &str) -> Result<(), String> {
    Err("a vm-host runs on macOS only".into())
}

#[cfg(target_os = "macos")]
fn boot_proof(options: &[&str]) -> Result<(), String> {
    let mut memory_mb = 1024u64;
    let mut timeout = 60u64;
    let mut iter = options.iter();
    while let Some(option) = iter.next() {
        let value = iter
            .next()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(USAGE)?;
        match *option {
            "--memory-mb" => memory_mb = value,
            "--timeout-s" => timeout = value,
            _ => return Err(USAGE.into()),
        }
    }
    if memory_mb == 0 || timeout == 0 {
        return Err(USAGE.into());
    }
    let memory_bytes = memory_mb
        .checked_mul(1024 * 1024)
        .ok_or("--memory-mb is too large")?;
    crate::vm::boot_proof(memory_bytes, std::time::Duration::from_secs(timeout))
}

#[cfg(not(target_os = "macos"))]
fn boot_proof(_options: &[&str]) -> Result<(), String> {
    Err("the boot proof runs on macOS only".into())
}

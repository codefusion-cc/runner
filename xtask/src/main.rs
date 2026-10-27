//! `cargo xtask <task>`: the build tasks of this repository.
//!
//! - `keygen <file>`: makes the image signing key; writes the secret seed to `<file>` (mode 0600) and prints the
//!   public key to commit as `crates/image/trusted-key.txt`.
//! - `guest`: builds the kernel and initramfs of the host's architecture into `target/guest`, signed with a key made
//!   for that build.
//! - `release --version V --out DIR --rootfs FILE (--key-file F | --ephemeral) [--guest DIR] [--arch A]
//!   [--max-part BYTES]`: the signed release folder with the root filesystem in parts, as the image workflow
//!   publishes it, verified before the task ends. `--guest` is where the kernel and initramfs are (default
//!   `target/guest`), `--arch` what they are built for (default the host's).
//! - `kernel check`: whether our copy of each architecture's pinned kernel package and Alpine still give it, and
//!   whether Alpine has a newer one. Fails when our copy of either does not.
//! - `cloud-hypervisor`: the Linux monitor pinned for the host's architecture into `target/vmm-programs`, where
//!   `cf-vmm-ch` embeds it.
//! - `cloud-hypervisor check`: whether our copy of each architecture's pinned monitor and the project's release
//!   still give it, and whether the project has a newer one. Fails when our copy of either does not.
//! - `fakes serve --code-file F --urls-file F [--timeout-s N]`: GitHub and the console as fakes, for a fresh host
//!   that `scripts/host-setup.sh` sets up: writes a connect code and their addresses, and ends once a host enrolled
//!   with it serves the fake's repository on GitHub tokens from the console alone (`xtask/src/fakes.rs`).
//! - `build`: the guest, then the release binary: on macOS ad hoc signed with the entitlement, on Linux with the
//!   monitor in it.

mod arch;
mod cloud_hypervisor;
mod cpio;
mod fakes;
mod guest;
mod it;
mod kernel;
mod keys;
mod pinned;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<()> {
    let root = workspace_root();
    match args.first().map(String::as_str) {
        Some("keygen") => keys::keygen(args.get(1).ok_or("usage: keygen <file>")?),
        Some("guest") => no_options(&args[1..]).and_then(|()| guest::build(&root)),
        Some("release") => guest::release(&root, &args[1..]),
        Some("build") => no_options(&args[1..]).and_then(|()| build(&root)),
        Some("it") => it::run(&root, &args[1..]),
        Some("fakes") => fakes::run(&args[1..]),
        Some("kernel") if args.get(1).map(String::as_str) == Some("check") => {
            kernel::check(&root, &args[2..])
        }
        Some("cloud-hypervisor") => cloud_hypervisor::run(&root, &args[1..]),
        _ => Err(
            "tasks: keygen, guest, release, build, it, fakes serve, kernel check, cloud-hypervisor [check]"
                .into(),
        ),
    }
}

fn no_options(args: &[String]) -> Result<()> {
    match args.first() {
        Some(option) => Err(format!("unknown option {option}").into()),
        None => Ok(()),
    }
}

fn build(root: &Path) -> Result<()> {
    guest::build(root)?;
    if cfg!(target_os = "linux") {
        cloud_hypervisor::fetch(root)?;
    }
    run_command(Command::new(cargo()).current_dir(root).args([
        "build",
        "--release",
        "-p",
        "codefusion-runner",
    ]))?;
    let binary = root.join("target/release/codefusion-runner");
    if !cfg!(target_os = "macos") {
        println!("built {}", binary.display());
        return Ok(());
    }
    run_command(
        Command::new("codesign")
            .args(["--force", "--sign", "-", "--entitlements"])
            .arg(root.join("crates/runner/runner.entitlements"))
            .arg(&binary),
    )?;
    println!("built and ad hoc signed {}", binary.display());
    Ok(())
}

pub fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

pub fn run_command(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{command:?} failed with {status}").into())
    }
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask sits in the workspace")
        .to_path_buf()
}

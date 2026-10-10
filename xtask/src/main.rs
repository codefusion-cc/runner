//! `cargo xtask <task>`: the build tasks of this repository.
//!
//! - `keygen <file>`: makes the image signing key; writes the secret seed to `<file>` (mode 0600) and prints the
//!   public key to commit as `crates/image/trusted-key.txt`.
//! - `guest`: builds the kernel and initramfs into `target/guest`, signed with a key made for that build.
//! - `release --version V --out DIR --rootfs FILE (--key-file F | --ephemeral) [--guest DIR] [--max-part BYTES]`:
//!   the signed release folder with the root filesystem in parts, as the image workflow publishes it, verified
//!   before the task ends. `--guest` is where the kernel and initramfs are (default `target/guest`).
//! - `build`: the guest, then the release binary, ad hoc signed with the entitlement.

mod cpio;
mod guest;
mod it;
mod kernel;
mod keys;

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
        _ => Err("tasks: keygen, guest, release, build, it".into()),
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
    run_command(Command::new(cargo()).current_dir(root).args([
        "build",
        "--release",
        "-p",
        "codefusion-runner",
    ]))?;
    let binary = root.join("target/release/codefusion-runner");
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

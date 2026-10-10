//! The CodeFusion Runner: `version`, `image verify` and `image install`, `vm` (the vm-host of one job) and
//! `boot-proof`.

mod commands;
#[cfg(target_os = "macos")]
mod vm;

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match commands::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("codefusion-runner: {message}");
            ExitCode::FAILURE
        }
    }
}

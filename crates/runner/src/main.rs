//! The CodeFusion Runner: the service of one Mac (`daemon`, `enroll`, `check`, `install`, `uninstall`), `version`,
//! `image verify` and `image install`, `vm` (the vm-host of one job) and `boot-proof`. On Linux, a machine's monitor
//! runs through the hidden `vm-sandbox`.

mod commands;
#[cfg(target_os = "macos")]
mod config;
#[cfg(target_os = "macos")]
mod hidden;
#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "macos")]
mod service;
#[cfg(target_os = "macos")]
mod update;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod vm;

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
        if args
            .first()
            .is_some_and(|first| first == cf_vmm_ch::sandbox::SUBCOMMAND)
        {
            let code = cf_vmm_ch::sandbox::main(&args[1..]);
            return ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX));
        }
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    match commands::run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("codefusion-runner: {message}");
            ExitCode::FAILURE
        }
    }
}

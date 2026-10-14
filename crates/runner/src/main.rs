//! The CodeFusion Runner: the service of one host (`daemon`, `enroll`, `check`, `install`, `uninstall`), `version`,
//! `image verify` and `image install`, `vm` (the vm-host of one job) and `boot-proof`, on macOS and on Linux. On
//! Linux, a machine's monitor runs through the hidden `vm-sandbox`, and `vmm` names the monitor the build carries.

mod commands;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod config;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod hidden;
#[cfg(target_os = "macos")]
mod launchd;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod manager;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod service;
#[cfg(target_os = "linux")]
mod systemd;
#[cfg(any(target_os = "macos", target_os = "linux"))]
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

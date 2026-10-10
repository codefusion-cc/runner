//! The CodeFusion Runner. M0 knows three commands: `version`, `image verify` and `boot-proof`.

mod commands;

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

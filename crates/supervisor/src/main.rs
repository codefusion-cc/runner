//! The supervisor, PID 1 of the guest. See `cf_supervisor` for what it decides and `linux` for how it acts.
//!
//! One binary, five ways to start it: with no argument it is the machine's first process; `container` is the
//! launcher it starts for a job's container; `docker` the launcher of a Docker job's sandbox, and
//! `docker-sandbox` the sandbox's first program; `hook <name>` is what the Actions runner's job hooks call.

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        ["container"] => linux::container::launch(),
        ["docker"] => linux::docker::launch(),
        ["docker-sandbox"] => linux::docker::sandbox(),
        ["hook", name] => linux::hook::run(name),
        _ => linux::init::run(),
    }
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the supervisor runs in the Linux guest only");
    std::process::exit(2);
}

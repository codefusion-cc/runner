//! What the Actions runner's job hooks print: the job's size at its start, and at its end why the Runner or the
//! kernel stopped something. Runs in the container, as the job's user.

use cf_guest_proto::{OomEvent, StopReason};
use cf_supervisor::docker;
use cf_supervisor::tell::{self, JobFacts};
use serde::de::DeserializeOwned;
use std::fs;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const SHARED: &str = "/run/codefusion";

fn read<T: DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_str(&fs::read_to_string(format!("{SHARED}/{name}")).ok()?).ok()
}

/// How long the start hook waits for a Docker job's daemon: it starts with the machine, and is up in a second or two.
const DOCKER_WAIT: Duration = Duration::from_secs(60);

/// Waits until a Docker job's daemon answers on its socket, and says so as an error on the run when it does not.
fn docker_ready() {
    let deadline = Instant::now() + DOCKER_WAIT;
    let why = loop {
        if let Ok(why) = fs::read_to_string(format!("{SHARED}/{}", docker::PROBLEM)) {
            break why;
        }
        if UnixStream::connect(docker::SOCKET).is_ok() {
            return;
        }
        if Instant::now() >= deadline {
            break format!(
                "its daemon did not answer within {} seconds.",
                DOCKER_WAIT.as_secs()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let log = fs::read_to_string(docker::LOG).unwrap_or_default();
    println!(
        "{}",
        tell::annotation("Docker did not start", &tell::docker_missing(why.trim(), &log))
    );
}

/// Prints what the hook `name` has to say. A hook that fails would fail the job, so this one never does.
pub fn run(name: &str) -> ! {
    let facts: Option<JobFacts> = read("job.json");
    match name {
        "job-started" => {
            if let Some(facts) = &facts {
                println!("{}", tell::started(facts));
                if facts.docker {
                    docker_ready();
                }
            }
        }
        "job-completed" => {
            if let Some(oom) = read::<OomEvent>("oom.json") {
                println!(
                    "{}",
                    tell::annotation("Out of memory", &tell::out_of_memory_line(&oom))
                );
                if let Some(mut summary) = std::env::var_os("GITHUB_STEP_SUMMARY")
                    .and_then(|path| fs::OpenOptions::new().append(true).create(true).open(path).ok())
                {
                    let _ = summary.write_all(tell::out_of_memory_summary(&oom).as_bytes());
                }
            }
            if let (Some(reason), Some(facts)) = (read::<StopReason>("stop.json"), &facts) {
                println!(
                    "{}",
                    tell::annotation("Stopped", &tell::stopped(reason, &facts.host))
                );
            }
        }
        _ => {}
    }
    std::process::exit(0)
}

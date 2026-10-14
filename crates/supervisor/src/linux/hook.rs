//! What the Actions runner's job hooks print: the job's size at its start, and at its end why the Runner or the
//! kernel stopped something. Runs in the container, as the job's user.

use super::sys;
use cf_guest_proto::{OomEvent, StopReason};
use cf_supervisor::docker;
use cf_supervisor::tell::{self, JobFacts};
use serde::de::DeserializeOwned;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const SHARED: &str = "/run/codefusion";

fn read<T: DeserializeOwned>(name: &str) -> Option<T> {
    serde_json::from_str(&fs::read_to_string(format!("{SHARED}/{name}")).ok()?).ok()
}

/// How long the start hook waits for a Docker job's daemon: it starts with the machine, and is up in a second or two.
const DOCKER_WAIT: Duration = Duration::from_secs(60);

/// Waits until a Docker job's daemon answers on its socket, and says so as an error on the run when it does not.
fn docker_ready() {
    let answer = sys::until(
        DOCKER_WAIT,
        Duration::from_millis(50),
        || match fs::read_to_string(format!("{SHARED}/{}", docker::PROBLEM)) {
            Ok(why) => Some(Some(why)),
            Err(_) => UnixStream::connect(docker::SOCKET).is_ok().then_some(None),
        },
    );
    let why = match answer {
        Some(None) => return,
        Some(Some(why)) => why,
        None => format!(
            "its daemon did not answer within {} seconds.",
            DOCKER_WAIT.as_secs()
        ),
    };
    let log = fs::read_to_string(docker::LOG).unwrap_or_default();
    println!(
        "{}",
        tell::annotation("Docker did not start", &tell::docker_missing(why.trim(), &log))
    );
}

/// How long the end hook gives the daemon to remove what the job left in it.
const TIDY_WAIT: Duration = Duration::from_secs(60);

/// Leaves a Docker job's cache as the next job wants it: without this job's containers, volumes and networks. Its
/// images and build cache stay; that is what the cache is for.
fn tidy_docker() {
    let deadline = Instant::now() + TIDY_WAIT;
    // The job's own daemon by its socket, whatever context or `DOCKER_HOST` the job chose.
    let host = format!("unix://{}", docker::SOCKET);
    let docker = |args: &[&str]| -> Option<String> {
        let mut child = Command::new("docker")
            .args(["--host", &host])
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let left = deadline.saturating_duration_since(Instant::now());
        if sys::until(left, Duration::from_millis(50), || {
            child.try_wait().ok().flatten()
        })
        .is_none()
        {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        let mut out = String::new();
        child.stdout.take()?.read_to_string(&mut out).ok()?;
        Some(out)
    };
    let containers: Vec<String> = docker(&["ps", "--all", "--quiet"])
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if !containers.is_empty() {
        let mut remove = vec!["rm", "--force", "--volumes"];
        remove.extend(containers.iter().map(String::as_str));
        docker(&remove);
    }
    docker(&["volume", "prune", "--all", "--force"]);
    docker(&["network", "prune", "--force"]);
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
            if facts.as_ref().is_some_and(|facts| facts.docker_cache) {
                tidy_docker();
            }
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

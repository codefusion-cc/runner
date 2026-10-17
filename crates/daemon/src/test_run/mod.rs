//! The console's `test-run` command: a developer's test suite run in one job machine, the image and the user CI jobs
//! get, so that a coding session runs its heavy suites on CodeFusion's machines instead of a laptop.
//!
//! The Runner's part is small: it checks the command's arguments, takes a slot from the budget as a job GitHub
//! assigned would (urgent, so it goes before the CI jobs that wait, and never stops one that runs), and starts a
//! machine whose workload is [`AGENT`], a Node script embedded in the Runner. The agent talks to the console by
//! itself: it fetches the suite's commit as git bundles, runs the suite and sends its output, so the Runner never
//! sees the code or the output. The machine is one of [`crate::Machines`]' like any other, with no Actions runner and
//! no registration at GitHub: its start is the agent's `ready`, its end the machine's.
//!
//! The wire contract is the console's (`remote-test-run-contract.md` with the owner's notes); the agent's half is
//! written down at the top of `agent.mjs`.

use std::time::Duration;

use cf_budget::{Size, Turn};
use cf_guest_proto::Workload;
use cf_secret::Secret;
use serde_json::Value;

use crate::traits::{Claim, Slots, Ticket};

/// The agent the machine runs: a Node script with no packages, only `node:` modules and `fetch`.
pub const AGENT: &str = include_str!("agent.mjs");

/// Node in the image: `/usr/local/bin/node` links to the tool cache's Node 24. The supervisor `execve`s the
/// workload's first word as it is, with no search of the PATH, so it is an absolute path.
pub const NODE: &str = "/usr/local/bin/node";

/// The scale set a test run's record names: none, since it has no registration at GitHub. No scale set GitHub
/// numbers is negative, and the Runner's own Actions runners outside a scale set are 0 ([`crate::held::OWN`]).
pub const SET: i64 = -1;

/// The workflow a test run's job events name.
pub const WORKFLOW: &str = "test-remote";

/// The event a test run's job events name.
pub const EVENT: &str = "test-run";

/// The repository a test run takes its turn in line as: all test runs are one, which goes before CI jobs that wait,
/// as a main-branch job does.
pub const TURN_REPO: &str = "test-run";

/// How long past the suite's own time a machine may stand: the setup (10 minutes at most), the output that is left
/// to send and the end.
pub const SLACK: Duration = Duration::from_secs(900);

/// How often a test run that waits for room asks whether its turn came.
pub const WAIT_POLL: Duration = Duration::from_millis(500);

/// The bounds of the command's arguments, as the console's contract gives them.
const TIMEOUT_S: std::ops::RangeInclusive<u64> = 60..=7200;
const WAIT_S: std::ops::RangeInclusive<u64> = 0..=3600;
const REPO_MAX: usize = 200;
const LABEL_MAX: usize = 300;

/// One test run as the console asked for it, checked.
#[derive(Debug, Clone)]
pub struct TestRun {
    /// The console's id of the run: `tr_` and 26 lowercase base32 characters.
    pub run: String,
    /// Where the agent talks to the console: the Runner's own console, `/agent/runners/runs/` and the run.
    pub url: String,
    /// The run's own token, for the agent alone: it reaches the machine's environment and nothing the Runner keeps.
    pub token: Secret<String>,
    /// The machine's size.
    pub size: Size,
    /// Whether the machine gets a Docker daemon, for a suite that runs in an image of its own.
    pub docker: bool,
    /// How long the suite may run, in seconds.
    pub timeout_s: u64,
    /// How long the run may wait for room, in seconds; 0 to take room now or none.
    pub wait_s: u64,
    /// The developer's repository, `owner/name`, for the job events.
    pub repo: String,
    /// What the developer calls the run, for the job events: the branch and the command.
    pub label: String,
}

impl TestRun {
    /// Reads the command's arguments, and says in a sentence for the console what is wrong with them when anything
    /// is. `console` is the address of the console this Runner reports to: the run's `url` must be on it, so a
    /// command can never send a run's token anywhere else.
    pub fn from_args(args: Option<&Value>, console: &str) -> Result<TestRun, String> {
        let args = args
            .and_then(Value::as_object)
            .ok_or("the command has no arguments")?;
        let text = |key: &str| -> Result<&str, String> {
            args.get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("`{key}` is not a text"))
        };
        let number = |key: &str, range: std::ops::RangeInclusive<u64>| -> Result<u64, String> {
            args.get(key)
                .and_then(Value::as_u64)
                .filter(|n| range.contains(n))
                .ok_or_else(|| {
                    format!(
                        "`{key}` is not a whole number from {} to {}",
                        range.start(),
                        range.end()
                    )
                })
        };
        let run = text("run")?;
        if !is_run(run) {
            return Err("`run` is not `tr_` and 26 lowercase base32 characters".into());
        }
        let url = text("url")?;
        let expected = format!("{}/agent/runners/runs/{run}", console.trim_end_matches('/'));
        if console.is_empty() || url != expected {
            return Err("`url` is not this run's address on the console this Runner reports to".into());
        }
        let token = text("token")?;
        if !is_token(token) {
            return Err("`token` is not `cfrt_` and 43 base64url characters".into());
        }
        let size = text("size")?;
        let size = Size::ALL
            .into_iter()
            .find(|s| s.label() == size)
            .ok_or("`size` is not cf-2g, cf-4g, cf-6g or cf-8g")?;
        let docker = match args.get("docker") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(docker)) => *docker,
            Some(_) => return Err("`docker` is not true or false".into()),
        };
        let timeout_s = number("timeoutS", TIMEOUT_S)?;
        let wait_s = number("waitS", WAIT_S)?;
        let repo = text("repo")?;
        if repo.is_empty() || repo.chars().count() > REPO_MAX || repo.chars().any(char::is_control) {
            return Err(format!(
                "`repo` is empty, longer than {REPO_MAX} characters or has control characters"
            ));
        }
        let label = text("label")?;
        if label.chars().count() > LABEL_MAX || label.chars().any(char::is_control) {
            return Err(format!(
                "`label` is longer than {LABEL_MAX} characters or has control characters"
            ));
        }
        Ok(TestRun {
            run: run.to_string(),
            url: url.to_string(),
            token: Secret::new(token.to_string()),
            size,
            docker,
            timeout_s,
            wait_s,
            repo: repo.to_string(),
            label: label.to_string(),
        })
    }

    /// The machine's name, which the job events give as the Actions runner's: `tr-` and the run's own characters.
    /// The same run always gets the same name, so a command the console sends again finds the machine it started.
    pub fn runner(&self) -> String {
        format!("tr-{}", &self.run[3..])
    }

    /// The suite's time on a Runner whose jobs run for `longest` at most: what the console asked for, or less, so
    /// that the agent's own timeout and its end come before the machine's deadline ([`SLACK`] before it). A Runner
    /// whose longest job leaves less than the shortest time a run may ask for takes none.
    pub fn timeout_within(&self, longest: Duration) -> Result<u64, String> {
        let room = longest.saturating_sub(SLACK).as_secs();
        if room < *TIMEOUT_S.start() {
            return Err(format!(
                "this host's jobs run for {} s at most, too short for a test run",
                longest.as_secs()
            ));
        }
        Ok(self.timeout_s.min(room))
    }

    /// The longest the machine may stand: the suite's time and [`SLACK`], never past `longest`, the longest any job on
    /// this Runner may run, which the lifecycle's own backstop is set by.
    pub fn deadline(&self, longest: Duration) -> Duration {
        let timeout = self.timeout_within(longest).unwrap_or(self.timeout_s);
        (Duration::from_secs(timeout) + SLACK).min(longest)
    }

    /// What the machine's workload is told: where the run is, its token, and the suite's time on a Runner whose jobs
    /// run for `longest` at most. Nothing else of the Runner's reaches it, and no value shows in a debug print.
    pub fn env(&self, longest: Duration) -> Vec<(String, Secret<String>)> {
        let timeout = self.timeout_within(longest).unwrap_or(self.timeout_s);
        vec![
            ("CF_RUN_URL".into(), Secret::new(self.url.clone())),
            ("CF_RUN_TOKEN".into(), self.token.clone()),
            ("CF_RUN_TIMEOUT_S".into(), Secret::new(timeout.to_string())),
        ]
    }
}

/// The workload of a test run's machine: Node, given the agent on its command line as an ES module.
///
/// The supervisor `execve`s the workload with the job's environment and no standard input, so the script goes as an
/// argument: Linux takes one of up to 32 pages (128 KiB with 4 KiB pages) and the host's message to the guest up to
/// 1 MiB, and the agent is far below both (`the_agent_fits_on_a_command_line`). It holds no secret, so that `ps`
/// shows it does no harm; the token comes in the environment.
pub fn workload() -> Workload {
    Workload::Command {
        argv: vec![
            NODE.to_string(),
            "--input-type=module".to_string(),
            "-e".to_string(),
            AGENT.to_string(),
        ],
    }
}

fn is_run(run: &str) -> bool {
    run.len() == 29
        && run.starts_with("tr_")
        && run[3..]
            .bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
}

fn is_token(token: &str) -> bool {
    token.len() == 48
        && token.starts_with("cfrt_")
        && token[5..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// What the budget said to a test run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    /// A slot: start the machine on it now.
    Start(Ticket),
    /// A place in line, to be waited for with [`wait_turn`].
    Waits(Ticket),
    /// Not now: the Mac takes no new job, a test run is there already, or no room and no wait. The console's
    /// result is `busy: ` and this.
    Busy(String),
    /// Never on this Mac with these settings: `never: ` and this.
    Never(String),
}

/// Asks the budget for a test run's slot: as a job GitHub assigned, urgent, with no promise before it. Only a Mac that
/// takes new jobs is asked, and only while no other test run is on it (`other`: the one that is).
pub fn admit(slots: &impl Slots, run: &TestRun, other: Option<&str>) -> Admission {
    let (open, why) = slots.state();
    // Jobs GitHub assigned that wait for room close the Mac to every other claim, but they are what an urgent run
    // goes before: the claim decides.
    let in_line = why == cf_budget::Refusal::AssignedFirst { waiting: 0 }.name();
    if !open && !in_line {
        return Admission::Busy(format!("the host takes no new job ({why})"));
    }
    if let Some(other) = other {
        return Admission::Busy(format!("test run {other} is on this host"));
    }
    // As many Docker jobs as the Mac allows hold a slot: a test run is held to that cap as a CI job is.
    if run.docker && slots.docker_room() == 0 {
        return Admission::Busy(crate::DOCKER_FULL.to_string());
    }
    let turn = Turn {
        urgent: true,
        repo: TURN_REPO.to_string(),
        // Not GitHub's job: no room the Mac would refuse a claim, and none while someone is at it.
        refusable: true,
    };
    match slots.claim_in_turn(run.size, None, turn) {
        Claim::Start(ticket) => Admission::Start(ticket),
        Claim::Waits { ticket, why } if run.wait_s == 0 => {
            slots.withdraw(ticket);
            Admission::Busy(format!("no room now ({why})"))
        }
        Claim::Waits { ticket, .. } => Admission::Waits(ticket),
        Claim::Never(why) => Admission::Never(why),
    }
}

/// How a test run's wait for its place in line ended ([`wait_turn`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waited {
    /// Its place became a slot: start the machine on it.
    Ready,
    /// The wait ran out.
    RanOut,
    /// The console stopped asking for the run (it ended or was cancelled there).
    Withdrawn,
}

/// Waits for a test run's place in line to become a slot, for `wait` at most and while `wanted` says the console
/// still asks for the run. A place that did not become a slot is withdrawn, so nothing is left of it in line.
pub async fn wait_turn(
    slots: &impl Slots,
    ticket: Ticket,
    wait: Duration,
    wanted: impl Fn() -> bool,
) -> Waited {
    let until = tokio::time::Instant::now() + wait;
    loop {
        // Before the slot is looked at: a run nobody wants any more gives back even a slot that just opened.
        if !wanted() {
            slots.withdraw(ticket);
            return Waited::Withdrawn;
        }
        if slots.ready(ticket) {
            return Waited::Ready;
        }
        if tokio::time::Instant::now() >= until {
            slots.withdraw(ticket);
            return Waited::RanOut;
        }
        tokio::time::sleep(WAIT_POLL.min(until.saturating_duration_since(tokio::time::Instant::now()))).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_fits_on_a_command_line() {
        // One argument of `execve` is at most 32 pages; the guest's pages are 4 KiB at least. Half that, so the
        // agent never comes near it as it grows.
        assert!(AGENT.len() < 64 << 10, "the agent is {} bytes", AGENT.len());
        assert!(!AGENT.contains('\0'), "a NUL byte cannot be in an argument");
        let Workload::Command { argv } = workload() else {
            panic!("a command")
        };
        assert_eq!(argv[0], NODE);
        assert_eq!(argv[3], AGENT);
    }
}

//! The commands on the machine for its holds ([`cf_daemon::holds`]): `pause --hold <name>` sets one and with `--wait`
//! waits until no job runs, `resume --hold <name>` lifts one, `status` says what holds the host and what runs. They
//! run as the Runner's own user, whose state folder holds the holds, and need no console and no root.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use cf_daemon::holds::{self, Hold, LocalStatus};
use serde::Serialize;

use crate::config::Config;

/// The exit code of a `pause --wait` whose time ran out while jobs still ran (`EX_TEMPFAIL`): the hold stays set.
pub const TIMED_OUT: u8 = 75;

/// How long `pause --wait` waits by default, in minutes.
pub const DEFAULT_WAIT_MINUTES: u64 = 60;

/// The longest `--timeout`, in minutes: a day.
pub const MAX_WAIT_MINUTES: u64 = 24 * 60;

/// How often `pause --wait` looks.
const LOOK_EVERY: Duration = Duration::from_secs(1);

/// What a command on the machine was asked.
#[derive(Debug, PartialEq, Eq)]
pub enum Asked {
    Pause {
        hold: String,
        reason: Option<String>,
        wait: Option<Duration>,
    },
    Resume {
        hold: String,
    },
    Status {
        json: bool,
    },
}

impl Asked {
    /// Reads `pause`, `resume` or `status` with their options, after `--config <file>`. None for another command;
    /// an error for one of them asked wrongly.
    pub fn from_args(command: &str, options: &[&str]) -> Option<Result<Asked, String>> {
        if !matches!(command, "pause" | "resume" | "status") {
            return None;
        }
        Some(Self::read(command, options))
    }

    fn read(command: &str, options: &[&str]) -> Result<Asked, String> {
        let (mut hold, mut reason, mut wait, mut timeout, mut json) = (None, None, false, None, false);
        let mut iter = options.iter();
        while let Some(option) = iter.next() {
            let mut value = || {
                iter.next()
                    .map(|value| (*value).to_string())
                    .ok_or_else(|| format!("{option} needs a value"))
            };
            match (command, *option) {
                ("pause" | "resume", "--hold") if hold.is_none() => hold = Some(value()?),
                ("pause", "--reason") if reason.is_none() => reason = Some(value()?),
                ("pause", "--wait") if !wait => wait = true,
                ("pause", "--timeout") if timeout.is_none() => timeout = Some(value()?),
                ("status", "--json") if !json => json = true,
                _ => return Err(format!("{command} does not take {option} here")),
            }
        }
        match command {
            "status" => Ok(Asked::Status { json }),
            _ => {
                let hold = hold.ok_or_else(|| format!("{command} needs --hold <name>"))?;
                if !holds::is_name(&hold) {
                    return Err(format!(
                        "--hold {hold:?}: 1 to {} of a-z, 0-9 and -",
                        holds::NAME_MAX
                    ));
                }
                if command == "resume" {
                    return Ok(Asked::Resume { hold });
                }
                let minutes = match timeout {
                    None => DEFAULT_WAIT_MINUTES,
                    Some(_) if !wait => return Err("--timeout goes with --wait".into()),
                    Some(text) => text
                        .parse::<u64>()
                        .ok()
                        .filter(|minutes| (1..=MAX_WAIT_MINUTES).contains(minutes))
                        .ok_or_else(|| format!("--timeout {text}: whole minutes, 1 to {MAX_WAIT_MINUTES}"))?,
                };
                Ok(Asked::Pause {
                    hold,
                    reason,
                    wait: wait.then(|| Duration::from_secs(minutes * 60)),
                })
            }
        }
    }
}

/// Runs a command on the machine and says how it ended: 0 when done, [`TIMED_OUT`] when a wait ran out, 1 for any
/// other failure, said on standard error.
pub fn run(config: &Path, asked: Asked) -> ExitCode {
    let ended = (|| {
        let state = state_folder(config)?;
        match asked {
            Asked::Pause { hold, reason, wait } => pause(&state, &hold, reason.as_deref(), wait),
            Asked::Resume { hold } => resume(&state, &hold).map_err(Ended::Failed),
            Asked::Status { json } => status(&state, json).map_err(Ended::Failed),
        }
    })();
    code(ended)
}

/// The exit code of a command that ended so, its message said on standard error.
fn code(ended: Result<(), Ended>) -> ExitCode {
    match ended {
        Ok(()) => ExitCode::SUCCESS,
        Err(Ended::TimedOut(message)) => {
            eprintln!("codefusion-runner: {message}");
            ExitCode::from(TIMED_OUT)
        }
        Err(Ended::Failed(message)) => {
            eprintln!("codefusion-runner: {message}");
            ExitCode::FAILURE
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Ended {
    TimedOut(String),
    Failed(String),
}

/// The configuration's state folder, which must be there and this user's: a hold another user wrote would be one
/// the Runner cannot read, and root's would be one it cannot lift.
fn state_folder(config: &Path) -> Result<PathBuf, Ended> {
    use std::os::unix::fs::MetadataExt as _;
    let config = Config::load(config).map_err(Ended::Failed)?;
    let state = config.state_dir;
    let owner = std::fs::metadata(&state)
        .map_err(|error| Ended::Failed(format!("{}: {error}; is the Runner installed?", state.display())))?
        .uid();
    // SAFETY: no pointer; it only asks for this process's effective user.
    let me = unsafe { libc::geteuid() };
    if owner != me {
        return Err(Ended::Failed(format!(
            "{} belongs to user {owner}: run this as that user (sudo -u <the Runner's user> codefusion-runner …)",
            state.display()
        )));
    }
    Ok(state)
}

fn pause(state: &Path, name: &str, reason: Option<&str>, wait: Option<Duration>) -> Result<(), Ended> {
    let hold = holds::set(
        &holds::holds_dir(state),
        name,
        reason,
        std::time::SystemTime::now(),
    )
    .map_err(Ended::Failed)?;
    let Some(wait) = wait else {
        println!("held: {name}; the Runner takes no new job once it sees it, within seconds");
        return Ok(());
    };
    let deadline = Instant::now() + wait;
    loop {
        let look = look(state, &hold);
        match look {
            Look::Idle => {
                println!("held: {name}; no job runs");
                return Ok(());
            }
            Look::Busy(why) if Instant::now() >= deadline => {
                return Err(Ended::TimedOut(format!(
                    "{why} after {} minutes; the hold {name} stays set (resume --hold {name} lifts it)",
                    wait.as_secs() / 60
                )));
            }
            Look::Busy(_) => std::thread::sleep(LOOK_EVERY),
        }
    }
}

fn resume(state: &Path, name: &str) -> Result<(), String> {
    let was = holds::lift(&holds::holds_dir(state), name)?;
    let left = holds::read(&holds::holds_dir(state));
    match (was, left.is_empty()) {
        (false, true) => println!("{name} was not set; nothing holds the host"),
        (false, false) => println!("{name} was not set; still held by {}", holds::names(&left)),
        (true, true) => println!("lifted {name}; nothing holds the host"),
        (true, false) => println!("lifted {name}; still held by {}", holds::names(&left)),
    }
    Ok(())
}

/// What `pause --wait` sees.
#[derive(Debug, PartialEq, Eq)]
enum Look {
    /// No job runs and none may start.
    Idle,
    /// Something may still run, and what.
    Busy(String),
}

/// Whether nothing runs any more under `hold`. Only a fresh status the Runner wrote after it took up this very hold
/// says so: before it saw the hold a job could still start. A Runner that is not running starts nothing, but its
/// machines may stand until the next one adopts them: their records say.
fn look(state: &Path, hold: &Hold) -> Look {
    let status = holds::read_status(&holds::status_file(state));
    // A status the Runner stopped writing says nothing of now: one that ended with machines may have none left.
    let fresh = |status: &&LocalStatus| holds::fresh(&status.at, std::time::SystemTime::now());
    if let Some(status) = status.as_ref().filter(fresh).filter(|status| seen(status, hold)) {
        return if status.idle() {
            Look::Idle
        } else {
            Look::Busy(busy(status))
        };
    }
    match runner_runs(state) {
        Ok(true) => Look::Busy("the Runner has not taken up the hold yet".into()),
        Ok(false) => match machine_records(state) {
            0 => Look::Idle,
            n => Look::Busy(format!(
                "the Runner is not running and {n} machine{} it left may still stand",
                if n == 1 { "" } else { "s" }
            )),
        },
        Err(error) => Look::Busy(error),
    }
}

/// Whether the status was written after the Runner took up this hold.
fn seen(status: &LocalStatus, hold: &Hold) -> bool {
    status
        .holds
        .iter()
        .any(|seen| seen.name == hold.name && seen.since == hold.since)
}

fn busy(status: &LocalStatus) -> String {
    let mut parts = Vec::new();
    if !status.machines.is_empty() {
        let each: Vec<String> = status
            .machines
            .iter()
            .map(|machine| match &machine.size {
                Some(size) => format!("{} ({size})", machine.runner),
                None => machine.runner.clone(),
            })
            .collect();
        parts.push(format!(
            "{} job{} still run: {}",
            each.len(),
            if each.len() == 1 { "" } else { "s" },
            each.join(", ")
        ));
    }
    if status.coming > 0 {
        parts.push(format!("{} promised to GitHub", status.coming));
    }
    parts.join("; ")
}

/// Whether a Runner holds the state folder's lock. Asked only while no status names the hold: taking the lock for
/// that moment could keep a Runner that starts in the very same instant from starting, which its service manager
/// then starts again.
fn runner_runs(state: &Path) -> Result<bool, String> {
    let lock = match std::fs::File::open(state.join("runner.lock")) {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("{}: {error}", state.display())),
    };
    match lock.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(format!("{}: {error}", state.display())),
    }
}

/// How many machines' records the state folder keeps.
fn machine_records(state: &Path) -> usize {
    std::fs::read_dir(cf_daemon::records_dir(state)).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.ends_with(".json"))
            })
            .count()
    })
}

/// What `status --json` prints.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Said {
    /// The holds set now, from their files.
    holds: Vec<Hold>,
    /// Whether a Runner runs over the state folder.
    running: bool,
    /// What the Runner last wrote; none before its first.
    last_seen: Option<LocalStatus>,
}

fn status(state: &Path, json: bool) -> Result<(), String> {
    let set = holds::read(&holds::holds_dir(state));
    let last_seen = holds::read_status(&holds::status_file(state));
    // With a fresh status in hand the Runner runs; the lock is asked only without one.
    let running = match &last_seen {
        Some(status) if holds::fresh(&status.at, std::time::SystemTime::now()) => true,
        _ => runner_runs(state)?,
    };
    if json {
        let said = Said {
            holds: set,
            running,
            last_seen,
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&said).map_err(|error| error.to_string())?
        );
        return Ok(());
    }
    if set.is_empty() {
        println!("no hold");
    }
    for hold in &set {
        match &hold.reason {
            Some(reason) => println!("held: {} ({reason}) since {}", hold.name, hold.since),
            None => println!("held: {} since {}", hold.name, hold.since),
        }
    }
    match (&last_seen, running) {
        (_, false) => println!("the Runner is not running"),
        (None, true) => println!("the Runner runs and has said nothing yet"),
        (Some(status), true) => {
            match (status.open, &status.reason) {
                (true, _) => println!("takes jobs"),
                (false, Some(reason)) => println!("takes no new job: {reason}"),
                (false, None) => println!("takes no new job"),
            }
            if status.console_paused {
                println!("paused from the console");
            }
            if status.idle() {
                println!("no job runs");
            } else {
                println!("{}", busy(status));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asked(command: &str, options: &[&str]) -> Result<Asked, String> {
        Asked::from_args(command, options).expect("a command on the machine")
    }

    #[test]
    fn pause_resume_and_status_read_their_options_and_nothing_else() {
        assert_eq!(
            asked("pause", &["--hold", "bots"]),
            Ok(Asked::Pause {
                hold: "bots".into(),
                reason: None,
                wait: None
            })
        );
        assert_eq!(
            asked(
                "pause",
                &[
                    "--wait",
                    "--hold",
                    "stress",
                    "--reason",
                    "run 7",
                    "--timeout",
                    "5"
                ]
            ),
            Ok(Asked::Pause {
                hold: "stress".into(),
                reason: Some("run 7".into()),
                wait: Some(Duration::from_secs(300))
            })
        );
        assert_eq!(
            asked("pause", &["--hold", "bots", "--wait"]),
            Ok(Asked::Pause {
                hold: "bots".into(),
                reason: None,
                wait: Some(Duration::from_secs(60 * 60))
            }),
            "an hour by default"
        );
        assert_eq!(
            asked("resume", &["--hold", "bots"]),
            Ok(Asked::Resume { hold: "bots".into() })
        );
        assert_eq!(asked("status", &[]), Ok(Asked::Status { json: false }));
        assert_eq!(asked("status", &["--json"]), Ok(Asked::Status { json: true }));
        assert_eq!(Asked::from_args("install", &[]), None);
    }

    #[test]
    fn a_command_asked_wrongly_is_refused_with_why() {
        for (command, options, why) in [
            ("pause", &[][..], "needs --hold"),
            ("pause", &["--hold"][..], "needs a value"),
            ("pause", &["--hold", "Bots"][..], "a-z"),
            ("pause", &["--hold", "../pause"][..], "a-z"),
            ("pause", &["--hold", &"a".repeat(33)][..], "a-z"),
            ("pause", &["--hold", "a", "--hold", "b"][..], "does not take"),
            (
                "pause",
                &["--hold", "a", "--timeout", "5"][..],
                "goes with --wait",
            ),
            (
                "pause",
                &["--hold", "a", "--wait", "--timeout", "0"][..],
                "1 to 1440",
            ),
            (
                "pause",
                &["--hold", "a", "--wait", "--timeout", "1441"][..],
                "1 to 1440",
            ),
            (
                "pause",
                &["--hold", "a", "--wait", "--timeout", "1.5"][..],
                "whole minutes",
            ),
            ("pause", &["--hold", "a", "--json"][..], "does not take"),
            ("resume", &[][..], "needs --hold"),
            ("resume", &["--hold", "a", "--wait"][..], "does not take"),
            ("status", &["--hold", "a"][..], "does not take"),
        ] {
            let error = asked(command, options).expect_err("refused");
            assert!(error.contains(why), "{command} {options:?}: {error}");
        }
        assert!(asked("pause", &["--hold", "a", "--wait", "--timeout", "1440"]).is_ok());
    }

    fn hold(name: &str, since: &str) -> Hold {
        Hold {
            name: name.into(),
            reason: None,
            since: since.into(),
        }
    }

    fn status_with(set: Vec<Hold>, machines: usize, coming: usize) -> LocalStatus {
        LocalStatus {
            at: holds::moment(std::time::SystemTime::now()),
            pid: 1,
            version: "0.1.0+test".into(),
            open: set.is_empty(),
            reason: None,
            console_paused: false,
            holds: set,
            machines: (0..machines)
                .map(|i| holds::StatusMachine {
                    runner: format!("host-j{i}"),
                    size: Some("cf-4g".into()),
                    state: "running".into(),
                    started: "2026-11-10T12:00:00Z".into(),
                })
                .collect(),
            coming,
            sizes: vec![],
        }
    }

    fn folder_with(
        status: Option<&LocalStatus>,
        records: usize,
        locked: bool,
    ) -> (tempfile::TempDir, Option<std::fs::File>) {
        let folder = tempfile::tempdir().expect("a folder");
        if let Some(status) = status {
            holds::write_status(&holds::status_file(folder.path()), status).expect("written");
        }
        let dir = cf_daemon::records_dir(folder.path());
        std::fs::create_dir_all(&dir).expect("records");
        for i in 0..records {
            std::fs::write(dir.join(format!("j{i}.json")), "{}").expect("a record");
        }
        std::fs::write(dir.join("j9.json.tmp"), "half").expect("no record");
        let lock = locked.then(|| {
            let lock = std::fs::File::create(folder.path().join("runner.lock")).expect("the lock");
            lock.lock().expect("locked");
            lock
        });
        (folder, lock)
    }

    #[test]
    fn a_wait_ends_only_on_a_status_written_after_the_runner_took_up_this_very_hold() {
        let mine = hold("bots", "2026-11-10T12:00:00.123Z");
        // The Runner runs and has not seen it, or saw an older hold of the same name: something may still start.
        for status in [
            None,
            Some(status_with(vec![], 0, 0)),
            Some(status_with(vec![hold("bots", "2026-11-10T11:00:00Z")], 0, 0)),
            Some(status_with(
                vec![hold("stress", "2026-11-10T12:00:00.123Z")],
                0,
                0,
            )),
        ] {
            let (folder, _lock) = folder_with(status.as_ref(), 0, true);
            assert_eq!(
                look(folder.path(), &mine),
                Look::Busy("the Runner has not taken up the hold yet".into()),
                "{status:?}"
            );
        }
        // It saw it: jobs that run, or slots promised to GitHub, keep the wait going.
        let (folder, _lock) = folder_with(Some(&status_with(vec![mine.clone()], 2, 0)), 2, true);
        assert_eq!(
            look(folder.path(), &mine),
            Look::Busy("2 jobs still run: host-j0 (cf-4g), host-j1 (cf-4g)".into())
        );
        let (folder, _lock) = folder_with(Some(&status_with(vec![mine.clone()], 0, 1)), 0, true);
        assert_eq!(
            look(folder.path(), &mine),
            Look::Busy("1 promised to GitHub".into())
        );
        let (folder, _lock) = folder_with(
            Some(&status_with(vec![mine.clone(), hold("a", "x")], 0, 0)),
            0,
            true,
        );
        assert_eq!(look(folder.path(), &mine), Look::Idle);
    }

    #[test]
    fn a_status_the_runner_stopped_writing_ends_no_wait() {
        let mine = hold("bots", "2026-11-10T12:00:00.123Z");
        let mut stale = status_with(vec![mine.clone()], 1, 0);
        stale.at = holds::moment(std::time::SystemTime::now() - Duration::from_secs(60));
        // A Runner runs, but not the one that wrote it: what it said then is not what runs now.
        let (folder, _lock) = folder_with(Some(&stale), 1, true);
        assert_eq!(
            look(folder.path(), &mine),
            Look::Busy("the Runner has not taken up the hold yet".into())
        );
        // None runs and its machines are gone: nothing runs, whatever the old status said.
        let (folder, _) = folder_with(Some(&stale), 0, false);
        assert_eq!(look(folder.path(), &mine), Look::Idle);
    }

    #[test]
    fn with_no_runner_running_a_wait_ends_once_no_machine_it_left_may_stand() {
        let mine = hold("bots", "2026-11-10T12:00:00.123Z");
        let (folder, _) = folder_with(None, 0, false);
        assert_eq!(look(folder.path(), &mine), Look::Idle);
        let (folder, _) = folder_with(None, 2, false);
        assert_eq!(
            look(folder.path(), &mine),
            Look::Busy("the Runner is not running and 2 machines it left may still stand".into())
        );
        // No lock file at all: never installed or never started.
        let folder = tempfile::tempdir().expect("a folder");
        assert_eq!(look(folder.path(), &mine), Look::Idle);
    }

    #[test]
    fn a_wait_that_runs_out_exits_75_and_keeps_the_hold() {
        let (folder, _lock) = folder_with(None, 0, true);
        let ended = pause(folder.path(), "bots", Some("bot run"), Some(Duration::ZERO));
        let Err(Ended::TimedOut(message)) = ended else {
            panic!("{ended:?}");
        };
        assert!(message.contains("resume --hold bots"), "{message}");
        assert_eq!(holds::read(&holds::holds_dir(folder.path()))[0].name, "bots");
    }

    #[test]
    fn a_wait_that_ran_out_exits_75_apart_from_every_other_failure() {
        assert_eq!(code(Ok(())), ExitCode::SUCCESS);
        assert_eq!(code(Err(Ended::TimedOut("jobs run".into()))), ExitCode::from(75));
        assert_eq!(code(Err(Ended::Failed("no folder".into()))), ExitCode::FAILURE);
        assert_ne!(ExitCode::FAILURE, ExitCode::from(TIMED_OUT));
    }

    #[test]
    fn resuming_a_hold_not_set_succeeds() {
        let folder = tempfile::tempdir().expect("a folder");
        assert_eq!(resume(folder.path(), "bots"), Ok(()));
        pause(folder.path(), "bots", None, None).expect("held");
        pause(folder.path(), "stress", None, None).expect("held");
        assert_eq!(resume(folder.path(), "bots"), Ok(()));
        assert_eq!(resume(folder.path(), "bots"), Ok(()));
        assert_eq!(
            holds::names(&holds::read(&holds::holds_dir(folder.path()))),
            "stress"
        );
    }
}

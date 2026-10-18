//! Whether a Docker daemon starts in this host's job machines, found by booting one.
//!
//! A host whose job machines cannot start Docker must get nothing that needs it: no `cf-docker` label set is served
//! and a test run with an image is answered `never:`, so the console hands it on to another machine. Nothing tells
//! that from outside a machine (on a Linux host the guest's Docker sandbox may end at once while the Macs' work), so
//! the Runner boots one small machine with Docker, through the budget like any job, whose workload waits for
//! `docker info` and says whether it answered. Until the first check ends, Docker is unknown and nothing is refused;
//! a pass holds for as long as the Runner runs (an update starts it again, and with it the check); a failure is
//! checked again every [`RECHECK_AFTER`]. A check machine that the host or someone ended before its workload could
//! answer (the host slept or ran short, it was stopped) says nothing of Docker: the check boots again after
//! [`AGAIN_AFTER_NO_ANSWER`] and what the last one found stands meanwhile.

use std::time::Duration;

use cf_guest_proto::Workload;
use cf_vmhost::{End as VmEnd, JobState};

use crate::EndKind;

/// How long after a failed check the next one boots.
pub const RECHECK_AFTER: Duration = Duration::from_secs(6 * 3600);

/// How long after a check machine that ended without an answer ([`DockerCheck::of_end`] says none) the next one boots.
pub const AGAIN_AFTER_NO_ANSWER: Duration = Duration::from_secs(10 * 60);

/// The longest `why` the console keeps of a check (it cuts what is longer).
pub const WHY_MAX: usize = 300;

/// How long the check's machine may stand: the boot, the two minutes the daemon gets, and a margin.
pub const DEADLINE: Duration = Duration::from_secs(300);

/// What the console is told when it asks for Docker on a host whose check failed, before the check's own words.
pub const REFUSAL: &str = "Docker does not start in this machine's job VMs";

/// The check's workload: two minutes for `docker info` to answer, as a test run's setup gives the daemon, then 0,
/// or 1 with the last answer in the console log.
const SCRIPT: &str = "for _ in $(seq 1 120); do docker info >/dev/null 2>&1 && exit 0; sleep 1; done; \
                      docker info >&2; exit 1";

/// The words in the guest's console log that say its Docker sandbox ended (`cf-supervisor`'s own line).
pub const SANDBOX_ENDED: &str = "Docker sandbox ended";

/// How much of the end of the check machine's console log is read for [`SANDBOX_ENDED`].
pub const CONSOLE_TAIL: usize = 64 << 10;

/// The workload of the check's machine.
pub fn workload() -> Workload {
    Workload::Command {
        argv: vec!["/bin/bash".into(), "-c".into(), SCRIPT.into()],
    }
}

/// What the last check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerCheck {
    /// Whether the daemon answered.
    pub ok: bool,
    /// When the check's machine ended, in seconds since 1970.
    pub at: i64,
    /// Why it failed, in words: how the check ended and, when the console log says so, that the sandbox ended.
    pub why: Option<String>,
}

impl DockerCheck {
    /// What a check machine's end says: it passed only when its workload ended with 0, and none when the machine
    /// ended for something that is no answer of Docker's (the host slept or ran short of memory or disk, the Runner
    /// or someone stopped it, the host shut down or the Runner started again before it was up). `state` is the
    /// vm-host's last word, `kind` the Runner's end of the machine, `console` reads the end of its console log, which
    /// only a failure needs.
    pub fn of_end(
        state: Option<&JobState>,
        kind: EndKind,
        console: impl FnOnce() -> Option<String>,
        at: i64,
    ) -> Option<DockerCheck> {
        let ended = state.and_then(|state| state.end.as_ref());
        match ended.map(|ended| ended.end) {
            Some(VmEnd::Ok) => {
                return Some(DockerCheck {
                    ok: true,
                    at,
                    why: None,
                });
            }
            Some(VmEnd::EvictedMemory | VmEnd::EvictedDisk | VmEnd::HostSlept | VmEnd::Cancelled) => {
                return None;
            }
            None if no_answer(kind) => return None,
            _ => {}
        }
        let how = match ended {
            Some(ended) if ended.end == VmEnd::Failed => {
                match ended.summary.as_ref().and_then(|summary| summary.exit_code) {
                    Some(code) => format!("docker info did not answer in 2 minutes (exit {code})"),
                    None => "the check ended with a failure".to_string(),
                }
            }
            Some(ended) => format!(
                "the check's machine ended: {}{}",
                vm_end(ended.end),
                ended
                    .message
                    .as_ref()
                    .map(|m| format!(", {m}"))
                    .unwrap_or_default()
            ),
            None => format!("the check's machine ended: {}", kind.name()),
        };
        let console = console();
        let sandbox = console
            .as_deref()
            .and_then(|log| log.lines().rev().find(|line| line.contains(SANDBOX_ENDED)));
        let why = match sandbox {
            Some(line) => format!("{how}; {}", line.trim()),
            None => how,
        };
        Some(DockerCheck {
            ok: false,
            at,
            why: Some(why),
        })
    }

    /// Why Docker is refused on this host: a failed check's words. None while it passed or before any check.
    pub fn refusal(check: Option<&DockerCheck>) -> Option<String> {
        let check = check.filter(|check| !check.ok)?;
        Some(match &check.why {
            Some(why) => format!("{REFUSAL} ({why})"),
            None => REFUSAL.to_string(),
        })
    }

    /// Whether a check is due at `now` (seconds since 1970): none ended yet, or the last failed [`RECHECK_AFTER`]
    /// ago. A pass is not checked again.
    pub fn due(check: Option<&DockerCheck>, now: i64) -> bool {
        match check {
            None => true,
            Some(check) if check.ok => false,
            Some(check) => waited(check.at, now, RECHECK_AFTER),
        }
    }
}

/// Whether `wait` has passed from `since` to `now`, both in seconds since 1970.
pub(crate) fn waited(since: i64, now: i64, wait: Duration) -> bool {
    now.saturating_sub(since) >= i64::try_from(wait.as_secs()).unwrap_or(i64::MAX)
}

/// Whether a machine the vm-host said nothing of ended for something that is no answer of Docker's.
const fn no_answer(kind: EndKind) -> bool {
    matches!(
        kind,
        EndKind::EvictedMemory
            | EndKind::EvictedDisk
            | EndKind::HostSlept
            | EndKind::HostShutdown
            | EndKind::DaemonRestart
            | EndKind::Stopped
    )
}

const fn vm_end(end: VmEnd) -> &'static str {
    match end {
        VmEnd::Ok => "ok",
        VmEnd::Oom => "out of memory",
        VmEnd::EvictedMemory => "stopped for the host's memory",
        VmEnd::EvictedDisk => "stopped for the host's disk",
        VmEnd::Timeout => "over its time",
        VmEnd::GuestLost => "the guest was lost",
        VmEnd::HostSlept => "the host slept",
        VmEnd::Failed => "failed",
        VmEnd::Cancelled => "stopped",
        VmEnd::BootFailed => "it did not boot",
    }
}

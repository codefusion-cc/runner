//! What a vm-host says about its job, in `run/<job>.json`.

use crate::Layout;
use cf_guest_proto::{GuestEnd, OomEvent, StopReason, Summary, Usage};
use serde::{Deserialize, Serialize};
use std::fs;

/// Where a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// The machine is being prepared or boots; the workload has not started.
    Booting,
    /// The workload runs.
    Running,
    /// The job was told to end and has its time to.
    Stopping,
    /// The workload ended; the machine is on its way down.
    Done,
    /// The machine is being stopped by force.
    Killing,
    /// Everything of the job is gone. The state has its `end`.
    Ended,
}

/// How a job ended. The first seven are what the console counts jobs by; the rest are ends before or outside a
/// job's own run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum End {
    /// The workload ran and ended with 0.
    Ok,
    /// The kernel stopped a process of the job for memory.
    Oom,
    /// Stopped because the Mac ran low on memory.
    EvictedMemory,
    /// Stopped because the Mac ran low on disk space.
    EvictedDisk,
    /// Stopped at its deadline.
    Timeout,
    /// The guest stopped answering, or the machine went without a word.
    GuestLost,
    /// Stopped because the Mac slept through it.
    HostSlept,
    /// The workload ended with something else than 0.
    Failed,
    /// Stopped on request.
    Cancelled,
    /// The machine never got as far as starting the workload.
    BootFailed,
}

impl End {
    /// The end of a job stopped for this reason.
    pub fn of_stop(reason: StopReason) -> Self {
        match reason {
            StopReason::Cancelled => End::Cancelled,
            StopReason::Timeout => End::Timeout,
            StopReason::EvictedMemory => End::EvictedMemory,
            StopReason::EvictedDisk => End::EvictedDisk,
            StopReason::HostSlept => End::HostSlept,
        }
    }

    /// The end the guest's summary describes: a stop by the Runner comes before what it caused, and running out
    /// of memory before the exit code that followed from it.
    pub fn of_summary(summary: &Summary) -> Self {
        match summary.end {
            GuestEnd::Stopped(reason) => End::of_stop(reason),
            GuestEnd::Exited if summary.oom.is_some() => End::Oom,
            GuestEnd::Exited if summary.exit_code == Some(0) => End::Ok,
            GuestEnd::Exited => End::Failed,
        }
    }
}

/// A job's end, with what is known about it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ended {
    /// How it ended.
    pub end: End,
    /// What went wrong, in words, when the end alone does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// The guest's own account, when it gave one: exit code, peaks, the process stopped for memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    /// Seconds since 1970 (UTC) when it ended.
    pub at: u64,
}

/// A job, as its vm-host knows it. Holds nothing secret.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobState {
    /// The job's name on this Mac.
    pub job: String,
    /// The vm-host's process.
    pub pid: u32,
    /// Seconds since 1970 (UTC) when the vm-host started.
    pub started_at: u64,
    /// Where the job is.
    pub stage: Stage,
    /// The machine's memory, which the Mac holds for as long as the machine runs.
    pub vm_bytes: u64,
    /// The job's own memory limit inside the machine.
    pub limit_bytes: u64,
    /// The machine's CPUs.
    pub cpus: u32,
    /// The image the job runs in.
    pub image: String,
    /// How often the machine was booted (2 after a boot that did not answer in time).
    pub boots: u32,
    /// Milliseconds from the machine's start to the workload's start, once it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_ms: Option<u64>,
    /// What the job uses, as of the guest's last heartbeat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// The first process the kernel stopped for memory, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom: Option<OomEvent>,
    /// The end, once there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Ended>,
}

/// Whether a job ran out of memory. Three answers on purpose: what cannot be known is `Unknown` with the reason, and
/// never `No`. `No` is only said of a job whose guest gave its own account of the end, or whose workload never started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "kebab-case")]
pub enum OutOfMemory {
    /// The kernel stopped a process of the job for memory; this is the first one.
    Yes(OomEvent),
    /// The job ended and the guest, heard to the end, reported no such stop; or the workload never started.
    No,
    /// Nobody can say.
    Unknown {
        /// Why, in words for a person: the state could not be read, the job has no end yet, the guest was lost.
        reason: String,
    },
}

impl JobState {
    /// Whether this job ran out of memory, as far as this state knows. A stop the guest reported counts whatever
    /// happened afterwards. Without one, only an end the guest described itself rules it out: a guest that was lost
    /// or stopped by force may have run out of memory unheard, which is `Unknown`.
    pub fn out_of_memory(&self) -> OutOfMemory {
        if let Some(event) = self
            .oom
            .as_ref()
            .or_else(|| self.end.as_ref()?.summary.as_ref()?.oom.as_ref())
        {
            return OutOfMemory::Yes(event.clone());
        }
        let reason = match &self.end {
            None => "the job has no end on record: it still runs, or its vm-host ended without writing one"
                .to_string(),
            Some(Ended { summary: Some(_), .. })
            | Some(Ended {
                end: End::BootFailed, ..
            }) => return OutOfMemory::No,
            Some(Ended { end, message, .. }) => {
                let end = serde_json::to_string(end).unwrap_or_default();
                let said = message
                    .as_deref()
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default();
                format!(
                    "the guest gave no account of the job's end ({}){said}",
                    end.trim_matches('"')
                )
            }
        };
        OutOfMemory::Unknown { reason }
    }
}

/// Whether the job of this name ran out of memory, from its state file. This never fails and never guesses: a file
/// that is missing, unreadable or not a job's state is `Unknown` with that as the reason.
pub fn out_of_memory(layout: &Layout, job: &str) -> OutOfMemory {
    let path = layout.state(job);
    let unknown = |what: String| OutOfMemory::Unknown {
        reason: format!("the job's state ({}) {what}", path.display()),
    };
    match fs::read_to_string(&path) {
        Err(error) => unknown(format!("could not be read: {error}")),
        Ok(text) => match serde_json::from_str::<JobState>(&text) {
            Err(error) => unknown(format!("is not a job's state: {error}")),
            Ok(state) if state.job != job => unknown(format!("is of another job, {:?}", state.job)),
            Ok(state) => state.out_of_memory(),
        },
    }
}

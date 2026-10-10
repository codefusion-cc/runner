//! What a vm-host says about its job, in `run/<job>.json`.

use cf_guest_proto::{GuestEnd, OomEvent, StopReason, Summary, Usage};
use serde::{Deserialize, Serialize};

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

//! The daemon: scale sets per repository and size, the scaler that ties a job to a slot, the machines that join
//! the budget, the lifecycle and the vm-hosts, and the service that runs them all.
//!
//! The scaler is the Go manager's `scaler` package ported: a message from a scale set's poll becomes acquired jobs,
//! Actions runners and retired Actions runners. It asks for slots ([`Slots`]) and machines ([`Hosts`]) through
//! traits; [`Machines`] is both, on the real budget ([`cf_budget`]), the real lifecycle ([`cf_lifecycle`]) and the
//! vm-hosts ([`Vms`]: [`VmHosts`] in service, a stand-in in tests). [`Service`] finds what to serve, keeps a listener
//! per scale set, removes registrations that have to go and reports to the console.

mod adapters;
pub mod caches;
pub mod docker_check;
mod ends;
pub mod held;
mod log;
mod machines;
mod making;
pub mod names;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod platform;
mod queued;
mod records;
mod roster;
mod scaler;
mod service;
pub mod test_run;
#[doc(hidden)]
pub mod testing;
mod traits;
mod turns;
pub mod update;
mod vms;

pub use adapters::SetRegistry;
pub use ends::{EndKind, machine_end};
pub use log::{Lines, ROLL_AT};
pub use machines::{
    Answer, DOCKER_FULL, JobReport, MachineConfig, MachineFacts, MachineParts, MachineStatus, Machines,
    RESULT_WAIT, Registration, Reports, SizeRoom, Snapshot,
};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use platform::{HostMachines, HostService, HostSetup, Platform, This, host_service, records_dir};
pub use records::{
    Disagrees, DiskStore, Ending, GithubJob, MemoryStore, RULES, Record, SavedSlot, Store, TestRunFacts,
    Upgrade,
};
pub use roster::{JobInfo, Roster, Runner};
pub use scaler::{IDLE_GRACE, PROMISE_TTL, Scaler, ScalerConfig, WasOutOfMemory, reconcile};
pub use service::{
    Console, HostExtras, LoggedReports, Nudge, OutboxReports, STALL_WARN_AFTER, Service, ServiceConfig,
    ServiceParts, SetState, iso, job_event,
};
pub use traits::{
    Claim, EventKind, Events, HostError, Hosts, JobEvent, Jobs, Refusal, Registry, RunnerSpec, Slots, Ticket,
};
pub use turns::turn_of;
pub use vms::{Seen, VmError, VmHosts, Vms};

/// A moment in seconds since 1970; 0 for one before that.
pub(crate) fn secs(at: std::time::SystemTime) -> i64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

/// The moment of [`secs`]; one before 1970 is 1970.
pub(crate) fn time_of(secs: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(secs).unwrap_or(0))
}

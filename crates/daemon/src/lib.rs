//! The daemon: scale sets per repository and size, the scaler that ties a job to a slot, the machines that join
//! the budget, the lifecycle and the vm-hosts, and the service that runs them all.
//!
//! The scaler is the Go manager's `scaler` package ported: a message from a scale set's poll becomes acquired jobs,
//! Actions runners and retired Actions runners. It asks for slots ([`Slots`]) and machines ([`Hosts`]) through
//! traits; [`Machines`] is both, on the real budget ([`cf_budget`]), the real lifecycle ([`cf_lifecycle`]) and the
//! vm-hosts ([`Vms`]: [`VmHosts`] in service, a stand-in in tests). [`Service`] finds what to serve, keeps a listener
//! per scale set, removes registrations that have to go and reports to the console.

mod adapters;
mod ends;
mod log;
#[cfg(target_os = "macos")]
mod mac;
mod machines;
pub mod names;
mod records;
mod roster;
mod scaler;
mod service;
#[doc(hidden)]
pub mod testing;
mod traits;
mod vms;

pub use adapters::SetRegistry;
pub use ends::{EndKind, machine_end};
pub use log::{Lines, ROLL_AT};
#[cfg(target_os = "macos")]
pub use mac::{MacMachines, MacService, MacSetup, mac_service, records_dir};
pub use machines::{
    JobReport, MachineConfig, MachineFacts, MachineParts, MachineStatus, Machines, RESULT_WAIT, Registration,
    Reports, Snapshot,
};
pub use records::{DiskStore, Ending, GithubJob, MemoryStore, Record, SavedSlot, Store};
pub use roster::{JobInfo, Roster, Runner};
pub use scaler::{IDLE_GRACE, PROMISE_TTL, Scaler, ScalerConfig, WasOutOfMemory};
pub use service::{
    Console, HostExtras, LoggedReports, Nudge, OutboxReports, Service, ServiceConfig, ServiceParts, SetState,
    iso, job_event,
};
pub use traits::{
    Claim, EventKind, Events, HostError, Hosts, JobEvent, Jobs, Refusal, Registry, RunnerSpec, Slots, Ticket,
};
pub use vms::{Seen, VmError, VmHosts, Vms};

/// A moment in seconds since 1970; 0 for one before that.
pub(crate) fn secs(at: std::time::SystemTime) -> i64 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_secs()).unwrap_or(i64::MAX))
}

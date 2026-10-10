//! The daemon's GitHub half: scale sets per repository and size, and the scaler that ties a claimed job to a slot.
//!
//! The scaler is the Go manager's `scaler` package ported: a message from a scale set's poll becomes acquired jobs,
//! Actions runners and retired Actions runners. The machine's slots ([`Slots`], the budget) and its virtual machines
//! ([`Hosts`], the VM monitor) are traits, so this crate depends on neither; the GitHub side is the real
//! [`cf_scaleset`] client.

mod adapters;
mod ends;
mod machines;
pub mod names;
mod records;
mod roster;
mod scaler;
#[doc(hidden)]
pub mod testing;
mod traits;
mod vms;

pub use adapters::SetRegistry;
pub use ends::{EndKind, machine_end};
pub use machines::{
    JobReport, MachineConfig, MachineFacts, MachineParts, MachineStatus, Machines, RESULT_WAIT, Registration,
    Reports, Snapshot,
};
pub use records::{DiskStore, Ending, GithubJob, MemoryStore, Record, SavedSlot, Store, replace_private};
pub use roster::{JobInfo, Roster, Runner};
pub use scaler::{IDLE_GRACE, PROMISE_TTL, Scaler, ScalerConfig, WasOutOfMemory};
pub use traits::{
    Claim, EventKind, Events, HostError, Hosts, JobEvent, Jobs, Refusal, Registry, RunnerSpec, Slots, Ticket,
};
pub use vms::{Seen, VmError, VmHosts, Vms};

//! The daemon's GitHub half: scale sets per repository and size, and the scaler that ties a claimed job to a slot.
//!
//! The scaler is the Go manager's `scaler` package ported: a message from a scale set's poll becomes acquired jobs,
//! runners and retired runners. The machine's slots ([`Slots`], the budget) and its virtual machines ([`Hosts`], the
//! VM monitor) are traits, so this crate depends on neither; the GitHub side is the real [`cf_scaleset`] client.

mod adapters;
pub mod names;
mod roster;
mod scaler;
#[doc(hidden)]
pub mod testing;
mod traits;

pub use adapters::SetRegistry;
pub use roster::{JobInfo, Roster, Runner};
pub use scaler::{IDLE_GRACE, Scaler, ScalerConfig, WasOutOfMemory};
pub use traits::{
    EventKind, Events, HostError, Hosts, JobEvent, Jobs, Refusal, Registry, RunnerSpec, Slots, Ticket,
};

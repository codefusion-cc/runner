//! The memory and CPU budget of one Mac: whether a job slot of a given size may open now.
//!
//! Pure: no clock, no system call, no thread. The caller reads the host once a second ([`Host`]) and hands each
//! reading to [`Budget::observe`] with the time; [`Budget::claim`] answers for one slot.
//!
//! # What holds, always
//!
//! - Every VM counts with its **full size from the moment its slot is reserved**, not with what it uses now.
//!   Virtualization.framework gives nothing back while a VM lives, so what a guest touched once stays taken.
//! - No claim is granted that takes the sum of VM sizes over the cap in force, or the sum of vCPUs over the CPU
//!   cap. Nothing is overcommitted.
//! - No claim is granted unless the Mac would keep its floor available even if every VM, the new one included,
//!   grew to its full size: `available - (promised - resident) - new >= floor`.
//! - No claim is granted unless the level is open: a fresh reading, no memory pressure, no swap-out for 30 s, the
//!   Mac not under its floor, cool, on mains, with disk, and all of that for 60 s without a break.
//! - A reading that fails, is old, or follows a gap in time (sleep, a stalled caller, a clock that went back)
//!   closes the Mac. Not knowing is never a reason to open.
//! - A slot's memory is given back once: a second [`Budget::release`] is an error and changes nothing.
//! - A job GitHub assigned without asking ([`Budget::assign`]) is never dropped and never started against the
//!   rules above, the person being there excepted: it waits for the Mac to have room, first in line.
//!
//! The numbers are [`Settings`]; [`Settings::for_host`] has the defaults and where each comes from.

mod budget;
mod host;
mod level;
mod settings;
mod size;

pub use budget::{
    Assigned, Budget, Charge, Eviction, EvictionCause, Free, Grant, Observed, Refusal, Released, SlotId,
    Status, UnknownSlot, Usage, admit,
};
pub use host::{Host, HostError, HostFacts, HostReading, Millis, Now, Pressure, Thermal};
pub use level::{Hold, Level, Trouble};
pub use settings::{Mode, Profile, Settings, SettingsError};
pub use size::Size;

/// One mebibyte. The plan's "MB" and "GB" are these: a 16 GB Mac reports 16 × 2^30 bytes.
pub const MIB: u64 = 1 << 20;
/// One gibibyte.
pub const GIB: u64 = 1 << 30;

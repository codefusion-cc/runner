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
//!   grew to its full size: `available - (promised - resident) - new >= floor`. What a VM holds counts as held
//!   only once a reading taken after that measurement shows it gone from `available`; less than before counts
//!   at once.
//! - No claim is granted unless the level is open: a fresh reading, no memory pressure, no swap-out for 30 s, the
//!   Mac not under its floor, cool, on mains, with disk, and all of that for 60 s without a break. The 60 s are
//!   counted from the first reading with nothing wrong to the newest one: the time since the newest reading is
//!   time nobody watched.
//! - A reading that fails, is old (by the time it was taken, not handed over), or follows a gap in time (sleep,
//!   a stalled caller, a clock that went back) closes the Mac. Not knowing is never a reason to open.
//! - A slot's memory is given back once: a second [`Budget::release`] is an error and changes nothing.
//! - A job GitHub assigned without asking ([`Budget::assign`]) is never dropped and never started against the
//!   rules above, the person being there excepted: it waits for the Mac to have room, first in line. Among the
//!   jobs that wait, main-branch and deploy jobs go first, repositories take turns, and none waits past
//!   [`DEFER_LIMIT`] behind a later one ([`Budget::line`]).
//!   [`Status`] names every slot with how it came to be ([`Origin`]) and every waiting job with what keeps it.
//!
//! The numbers are [`Settings`]; [`Settings::for_host`] has the defaults and where each comes from. [`FakeHost`] is a
//! host a test scripts: time passes, the host sleeps, the person comes back, a read fails.

mod budget;
mod fake;
mod host;
mod level;
mod settings;
mod size;

pub use budget::{
    Assigned, Budget, Charge, Eviction, EvictionCause, Free, Grant, Held, Observed, Origin, Refusal,
    Released, SlotId, Status, Turn, UnknownSlot, Usage, Waiting, admit,
};
pub use fake::FakeHost;
pub use host::{Battery, Host, HostError, HostFacts, HostReading, Millis, Now, Pressure, Thermal};
pub use level::{Hold, Level, Trouble};
pub use settings::{DEFER_LIMIT, Limits, Mode, Profile, Settings, SettingsError};
pub use size::Size;

/// One mebibyte. The plan's "MB" and "GB" are these: a 16 GB Mac reports 16 × 2^30 bytes.
pub const MIB: u64 = 1 << 20;
/// One gibibyte.
pub const GIB: u64 = 1 << 30;

/// `percent`% of `bytes`, rounded down, with no overflow on the way.
pub fn percent_of(bytes: u64, percent: u32) -> u64 {
    u64::try_from(u128::from(bytes) * u128::from(percent) / 100).unwrap_or(u64::MAX)
}

//! What the Runner reads from Linux, behind the budget's [`Host`](cf_budget::Host) trait. The Linux counterpart of
//! `cf-host-macos`, with the same names where the two do the same.
//!
//! - [`LinuxHost`] reads `/proc/meminfo` (`MemAvailable`, swap), `/proc/vmstat` (`pswpout`), PSI's
//!   `/proc/pressure/memory` when the kernel has it, the thermal zones and their trip points, the power supplies,
//!   and `statvfs` of the state folder. It only reads; nothing here changes the host. PSI's `cpu` and `io` files
//!   are not read: the budget decides by memory, heat, power and disk, and has no use for them.
//! - [`Raw`] is what Linux answered and [`reading`] what the budget gets from it, a pure function; [`parse`] reads
//!   the text of each file. Both are tested on every platform.
//! - For the console's page and the daemon: [`boot_session`], [`boot_time`], [`model`], [`battery`],
//!   [`host_name`], [`load`], [`cpu_percent`] with [`LinuxHost::cpu_ticks`], [`LinuxHost::disk_total_bytes`],
//!   and [`process`] and [`program_of`], which say which program a process runs and since when, so that a number
//!   that went to a later process is told apart. [`fast_cpus`] names the cores machines are pinned to.
//!
//! Every reader takes the folder its `/proc` and `/sys` are under, `/` on a real host, so the tests run against
//! folders of fixtures on any OS. What a value means where Linux and macOS differ is said at [`reading`]: there is
//! no person's idle time, and a host without a battery runs on the mains.

mod convert;
mod host;
pub mod parse;
mod read;

pub use convert::{Raw, Supply, Trip, TripKind, Zone, cpu_percent, pressure_of, reading, thermal_of};
pub use host::LinuxHost;
pub use parse::{CpuTicks, Psi};
pub use read::{
    Process, battery, boot_session, boot_time, fast_cpus, host_name, load, model, process, program_of,
};

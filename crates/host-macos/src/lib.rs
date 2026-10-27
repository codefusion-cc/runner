//! What the Runner reads from macOS, behind the budget's [`Host`](cf_budget::Host) trait.
//!
//! - [`MacHost`] asks macOS (only on macOS): `host_statistics64` for the page counts, `sysctl` for the pressure
//!   level and swap, the HID system for the time since the last key or mouse event, the thermal pressure
//!   notification, the power source and `statfs`. It only reads; nothing here changes the Mac. The battery's
//!   charge, which only the console's page shows, is what `pmset -g batt` prints ([`battery`]). [`program_of`] says
//!   which program a process runs.
//! - [`Raw`] is what macOS answered and [`reading`] what the budget gets from it, a pure function, so the
//!   arithmetic is tested on every platform with numbers taken from real Macs.
//! - [`exclude_from_backup`] marks a folder the Runner made as skipped by Time Machine, the way `tmutil addexclusion`
//!   does; [`exclude_with`] is the decision behind an [`Attributes`] edge a test scripts.
//!
//! # What "available" means
//!
//! [`HostReading::available_bytes`](cf_budget::HostReading::available_bytes) is free memory plus the file cache
//! plus purgeable memory: what Activity Monitor shows as physical memory minus "Memory Used", which macOS hands to
//! a VM without compressing anybody's pages. While the Mac is calm it also counts what compressing its programs'
//! inactive pages frees, which macOS does before it swaps: an idle 16 GB Mac mini held 5.4 GB so and 2.2 GB more
//! in inactive pages, and a 4 GB machine needs 6.2 GB with the floor.
//!
//! `kern.memorystatus_level` alone is no measure of it. That percentage counts every program's own pages that are
//! not compressed yet, active ones too (measured: (active + inactive + free + speculative) / usable memory), so it
//! read 64% and pressure "normal" on a 16 GB Mac that had 7.4 of 8.2 GB of swap in use and 3 GB available. It is
//! what says the Mac is calm ([`CALM_LEVEL`]), with a compressor that holds little ([`HEAVY_COMPRESSOR_SHARE`]),
//! and the most the count may reach.

mod backup;
mod convert;
#[cfg(target_os = "macos")]
mod mac;

#[cfg(target_os = "macos")]
pub use backup::exclude_from_backup;
pub use backup::{ATTRIBUTE, Attributes, Exclusion, VALUE, exclude_with};
pub use cf_budget::Battery;
pub use convert::{CALM_LEVEL, HEAVY_COMPRESSOR_SHARE, Raw, battery_in, model_name, reading};
#[cfg(target_os = "macos")]
pub use mac::{
    MacHost, battery, boot_session, boot_time, cpu_percent, host_name, load, model, performance_cores,
    program_of,
};

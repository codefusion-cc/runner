//! What the Runner reads from macOS, behind the budget's [`Host`](cf_budget::Host) trait.
//!
//! - [`MacHost`] asks macOS (only on macOS): `host_statistics64` for the page counts, `sysctl` for the pressure
//!   level and swap, the HID system for the time since the last key or mouse event, the thermal pressure
//!   notification, the power source and `statfs`. It only reads; nothing here changes the Mac. The battery's
//!   charge, which only the console's page shows, is what `pmset -g batt` prints ([`battery`]).
//! - [`Raw`] is what macOS answered and [`reading`] what the budget gets from it, a pure function, so the
//!   arithmetic is tested on every platform with numbers taken from real Macs.
//! - [`FakeHost`] is a host a test scripts: time passes, the Mac sleeps, the person comes back, a read fails.
//!
//! # What "available" means
//!
//! [`HostReading::available_bytes`](cf_budget::HostReading::available_bytes) is free memory plus the file cache
//! plus purgeable memory: what Activity Monitor shows as physical memory minus "Memory Used". It is what macOS can
//! hand to a VM without compressing or swapping anybody's pages.
//!
//! It is not `kern.memorystatus_level`. That percentage also counts every program's own pages that are not
//! compressed yet (measured: (active + inactive + free + speculative) / usable memory), so it read 64% and
//! pressure "normal" on a 16 GB Mac that had 7.4 of 8.2 GB of swap in use and 3 GB available.

mod convert;
mod fake;
#[cfg(target_os = "macos")]
mod mac;

pub use convert::{Battery, Raw, battery_in, model_name, reading};
pub use fake::FakeHost;
#[cfg(target_os = "macos")]
pub use mac::{MacHost, battery, boot_session, boot_time, cpu_percent, host_name, load, model};

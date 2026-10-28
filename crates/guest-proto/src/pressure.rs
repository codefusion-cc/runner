//! Pressure stall information (PSI): the share of the last 10 s that work waited for memory, a CPU or the disk, as
//! the kernel writes it in `/proc/pressure/*` and in each cgroup's `*.pressure` files.

use serde::{Deserialize, Serialize};

/// A share of time over the last 10 s from a pressure file, in hundredths of a percent: 1234 is 12.34%.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Psi {
    /// `some avg10`: at least one task was stalled on the resource.
    pub some: u32,
    /// `full avg10`: every task that wanted to run was stalled on it at once.
    pub full: u32,
}

impl Psi {
    /// The larger of each share: the worst of two readings.
    pub fn max(self, other: Psi) -> Psi {
        Psi {
            some: self.some.max(other.some),
            full: self.full.max(other.full),
        }
    }
}

/// What a job's cgroup stalled on: memory, its CPUs and the disk.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pressure {
    /// `memory.pressure`: reclaiming, or waiting for pages read back in. `full` near 100 is a job frozen for memory.
    pub memory: Psi,
    /// `cpu.pressure`: runnable but not running.
    pub cpu: Psi,
    /// `io.pressure`: waiting for the disk.
    pub io: Psi,
}

impl Pressure {
    /// The worst of each share of two readings.
    pub fn max(self, other: Pressure) -> Pressure {
        Pressure {
            memory: self.memory.max(other.memory),
            cpu: self.cpu.max(other.cpu),
            io: self.io.max(other.io),
        }
    }
}

/// Digits only, as a number; `None` for anything else, an empty text and a number past `u64`.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// `12.34` as 1234; at most 100.00.
fn hundredths(text: &str) -> Option<u32> {
    let (whole, part) = text.split_once('.').unwrap_or((text, "0"));
    if whole.len() > 3 || part.is_empty() || part.len() > 2 {
        return None;
    }
    let part = number(part)? * if part.len() == 1 { 10 } else { 1 };
    let value = number(whole)? * 100 + part;
    u32::try_from(value).ok().filter(|value| *value <= 10_000)
}

/// The `avg10` of a pressure file's `some` and `full` lines; `None` when a line or its `avg10` is missing or no share.
pub fn psi(text: &str) -> Option<Psi> {
    let avg10 = |kind: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(kind)?.strip_prefix(' '))?
            .split_whitespace()
            .find_map(|pair| pair.strip_prefix("avg10="))
            .and_then(hundredths)
    };
    Some(Psi {
        some: avg10("some")?,
        full: avg10("full")?,
    })
}

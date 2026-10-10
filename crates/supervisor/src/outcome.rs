//! How a container ended: what its launcher reports, and what the supervisor makes of the report.

use serde::{Deserialize, Serialize};

/// How the container ended, as the launcher writes it to the supervisor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The workload's wait status, as `waitpid` gives it.
    Status(i32),
    /// The container could not be made.
    Error(String),
}

/// The most of a report that is read; a report is one short line.
pub const REPORT_BYTES: u64 = 1 << 16;

/// The workload's exit code, or the signal that ended it, from the launcher's `report` and the launcher's own
/// end (`launcher`: its exit code and its signal). `Err` when the container could not be made, with the reason.
///
/// A report that is none (empty, cut short, or with more in it than a report) means the container went before
/// it could say, or something else wrote where only the launcher writes. Then the launcher's own end is what
/// there is, but never as a workload that ended well: an exit code of 0 with no report behind it is no exit code.
pub fn workload_end(
    report: &str,
    launcher: (Option<i32>, Option<i32>),
) -> Result<(Option<i32>, Option<i32>), String> {
    match serde_json::from_str::<Outcome>(report) {
        Ok(Outcome::Error(error)) => Err(error),
        // A wait status on Linux: the low seven bits are the signal, and none means the process exited with the
        // code in the next eight.
        Ok(Outcome::Status(status)) if status & 0x7f == 0 => Ok((Some((status >> 8) & 0xff), None)),
        Ok(Outcome::Status(status)) => Ok((None, Some(status & 0x7f))),
        Err(_) => Ok((launcher.0.filter(|code| *code != 0), launcher.1)),
    }
}

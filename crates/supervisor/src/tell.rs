//! What a job is told, in its own log: its size at the start, and why it ended when the Runner ended it.

use cf_guest_proto::{Limits, OomEvent, StopReason};
use serde::{Deserialize, Serialize};

/// What the hooks know about the job, written by the supervisor where the hook reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFacts {
    /// What the Mac is called.
    pub host: String,
    /// The job's limits.
    pub limits: Limits,
    /// The size of the job's disk.
    pub disk_bytes: u64,
    /// Whether the job has a Docker daemon.
    #[serde(default)]
    pub docker: bool,
}

/// A size as people write it: `3.75 GB`, `4 GB`, `512 MB`.
pub fn size(bytes: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    if bytes >= GB {
        let hundredths = (bytes as u128 * 100 / GB as u128) as u64;
        let text = format!("{}.{:02}", hundredths / 100, hundredths % 100);
        format!("{} GB", text.trim_end_matches('0').trim_end_matches('.'))
    } else {
        format!("{} MB", bytes / MB)
    }
}

/// The line at a job's start.
pub fn started(facts: &JobFacts) -> String {
    format!(
        "This job runs on {} with {} memory, {} CPUs, {} disk{}.",
        facts.host,
        size(facts.limits.memory_bytes),
        facts.limits.cpus,
        size(facts.disk_bytes),
        if facts.docker { " and Docker" } else { "" },
    )
}

/// The error a Docker job shows when its daemon is not there: why, and the end of what the daemon said.
pub fn docker_missing(why: &str, log: &str) -> String {
    let tail: Vec<&str> = log
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(5)
        .collect();
    let mut text = format!("Docker is not available in this job: {why}");
    if !tail.is_empty() {
        text.push_str(" The daemon's log ends with:");
        for line in tail.into_iter().rev() {
            text.push('\n');
            text.push_str(&line.chars().take(300).collect::<String>());
        }
    }
    text
}

/// What to do about a job that ran out of memory. The sizes a job can ask for are 2, 4, 6 and 8 GB; the limit is a
/// quarter of a gigabyte under the size.
fn advice(limit_bytes: u64) -> String {
    let size_gb = limit_bytes.div_ceil(1 << 30);
    match [4u64, 6, 8].into_iter().find(|larger| *larger > size_gb) {
        Some(larger) => {
            format!("Ask for more with `runs-on: cf-{larger}g`, or lower the number of test workers.")
        }
        None => "This is the largest size. Lower the number of test workers.".to_string(),
    }
}

/// The error a job shows when the kernel stopped one of its processes for memory: one line for the annotation.
pub fn out_of_memory_line(oom: &OomEvent) -> String {
    format!(
        "This job used all of its {} memory, so {} was stopped. {}",
        size(oom.limit_bytes),
        oom.process.name,
        advice(oom.limit_bytes)
    )
}

/// The same as a job summary (Markdown): the limit, the process stopped, who held the memory, what to do.
pub fn out_of_memory_summary(oom: &OomEvent) -> String {
    let mut text = format!(
        "### Out of memory\n\nThis job has {} memory and used all of it. `{}` (process {}) was stopped.\n",
        size(oom.limit_bytes),
        oom.process.name,
        oom.process.pid
    );
    if !oom.largest.is_empty() {
        text.push_str("\nThe largest processes at that moment:\n\n| Process | Memory |\n| --- | --- |\n");
        for process in &oom.largest {
            text.push_str(&format!(
                "| `{}` | {} |\n",
                process.name,
                size(process.memory_bytes)
            ));
        }
    }
    text.push_str(&format!("\n{}\n", advice(oom.limit_bytes)));
    text
}

/// Why the Runner ended a job that did nothing wrong, or ran too long.
pub fn stopped(reason: StopReason, host: &str) -> String {
    let because = match reason {
        StopReason::EvictedMemory => {
            format!("{host} ran low on memory because of other work on it. This job did nothing wrong.")
        }
        StopReason::EvictedDisk => {
            format!("{host} ran low on disk space. This job did nothing wrong.")
        }
        StopReason::HostSlept => format!("{host} was asleep for too long. This job did nothing wrong."),
        StopReason::Timeout => "the job ran longer than a job may.".to_string(),
        StopReason::Cancelled => "the job was cancelled.".to_string(),
    };
    format!("Stopped by the CodeFusion Runner: {because}")
}

/// An error annotation, as the workflow command the Actions runner reads from a step's or a hook's output. What
/// would end the command or its title early (a line break, `%`, and in the title `:` and `,`) is escaped.
pub fn annotation(title: &str, message: &str) -> String {
    let escape = |text: &str| text.replace('%', "%25").replace('\r', "%0D").replace('\n', "%0A");
    format!(
        "::error title={}::{}",
        escape(title).replace(':', "%3A").replace(',', "%2C"),
        escape(message)
    )
}

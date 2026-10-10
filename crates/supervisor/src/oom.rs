//! Reading what the kernel says when it stops a process for memory.
//!
//! The kernel log has, for each such stop, a table of the job's processes with their memory, then the line that
//! names the one it stopped. [`Log`] is fed the log's lines and gives one [`Kill`] per stop.

use cf_guest_proto::Process;

/// How many of the largest processes a report names.
pub const LARGEST: usize = 5;

/// One process stopped by the kernel, with the largest processes at that moment (largest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kill {
    /// The process that was stopped.
    pub process: Process,
    /// The processes that held the most memory, [`LARGEST`] at most.
    pub largest: Vec<Process>,
}

/// The message of one record of `/dev/kmsg` (`<priority>,<sequence>,<time>,<flags>;<message>`). `None` for a
/// continuation line, which carries no message.
pub fn message_of(record: &str) -> Option<&str> {
    if record.starts_with(' ') {
        return None;
    }
    let (_, message) = record.split_once(';')?;
    Some(message.lines().next().unwrap_or(""))
}

/// How often the kernel has stopped a process of a control group, from its `memory.events`.
pub fn kills_in_events(memory_events: &str) -> u64 {
    memory_events
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse().ok())
        .unwrap_or(0)
}

/// Collects the kernel's report line by line.
#[derive(Debug, Default)]
pub struct Log {
    /// The index of the `rss` column of the table being read, once its header was seen.
    rss_column: Option<usize>,
    columns: usize,
    table: Vec<Process>,
    page_bytes: u64,
}

impl Log {
    /// A reader for a kernel whose pages are `page_bytes` large (the table counts memory in pages).
    pub fn new(page_bytes: u64) -> Self {
        Self {
            page_bytes,
            ..Self::default()
        }
    }

    /// Takes one message of the kernel log. Returns the stop it completes, if any.
    pub fn line(&mut self, message: &str) -> Option<Kill> {
        let message = message.trim();
        if let Some(rest) = message.strip_prefix('[') {
            let (pid, columns) = rest.split_once(']')?;
            if pid.trim() == "pid" {
                // The header: `[ pid ] uid tgid total_vm rss ... oom_score_adj name`.
                let names: Vec<&str> = columns.split_ascii_whitespace().collect();
                self.rss_column = names.iter().position(|name| *name == "rss");
                self.columns = names.len();
                self.table.clear();
            } else if let (Some(rss_column), Ok(pid)) = (self.rss_column, pid.trim().parse::<u32>()) {
                // A row: numbers, then the name, which is the last column and may hold spaces.
                if let Some((numbers, name)) = split_row(columns, self.columns)
                    && let Some(Ok(pages)) = numbers.get(rss_column).map(|rss| rss.parse::<u64>())
                {
                    self.table.push(Process {
                        pid,
                        name: name.to_string(),
                        memory_bytes: pages.saturating_mul(self.page_bytes),
                    });
                }
            }
            return None;
        }
        let (pid, name) = parse_killed(message)?;
        let mut table = std::mem::take(&mut self.table);
        self.rss_column = None;
        table.sort_by(|a, b| b.memory_bytes.cmp(&a.memory_bytes).then(a.pid.cmp(&b.pid)));
        let memory_bytes = table
            .iter()
            .find(|process| process.pid == pid)
            .map_or(0, |process| process.memory_bytes);
        table.truncate(LARGEST);
        Some(Kill {
            process: Process {
                pid,
                name,
                memory_bytes,
            },
            largest: table,
        })
    }
}

/// `... out of memory: Killed process 1234 (node) total-vm:...`: the id and the name. The name may hold brackets.
fn parse_killed(message: &str) -> Option<(u32, String)> {
    if !message.contains("ut of memory") {
        return None;
    }
    let rest = &message[message.find("Killed process ")? + "Killed process ".len()..];
    let (pid, rest) = rest.split_once(" (")?;
    let end = rest.find(") total-vm").or_else(|| rest.rfind(')'))?;
    Some((pid.parse().ok()?, rest[..end].to_string()))
}

/// Splits a row of `columns` columns into its numbers and the rest, the name.
fn split_row(row: &str, columns: usize) -> Option<(Vec<&str>, &str)> {
    let mut numbers = Vec::new();
    let mut rest = row;
    for _ in 0..columns.checked_sub(1)? {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        numbers.push(&rest[..end]);
        rest = &rest[end..];
    }
    Some((numbers, rest.trim()))
}

//! Reading what the kernel says when it stops a process for memory.
//!
//! The kernel log has, for each such stop, a table of the job's processes with their memory, then the line that
//! names the one it stopped. [`Log`] is fed the log's lines and gives one [`Kill`] per stop.

use cf_guest_proto::{LARGEST_PROCESSES, Process};

/// One process stopped by the kernel, with the largest processes at that moment (largest first).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kill {
    /// The process that was stopped.
    pub process: Process,
    /// The processes that held the most memory, [`LARGEST_PROCESSES`] at most.
    pub largest: Vec<Process>,
}

/// The message of one record of `/dev/kmsg` (`<priority>,<sequence>,<time>,<flags>;<message>`) that the kernel
/// itself wrote. `None` for a continuation line, which carries no message, and for a record a process wrote into
/// the log: the kernel marks those with a facility of their own (a priority of 8 or more), and what a process
/// says about memory is not the kernel's word.
pub fn message_of(record: &str) -> Option<&str> {
    if record.starts_with(' ') {
        return None;
    }
    let (header, message) = record.split_once(';')?;
    let priority: u32 = header.split(',').next()?.parse().ok()?;
    if priority >> 3 != 0 {
        return None;
    }
    Some(message.lines().next().unwrap_or(""))
}

/// The most records of the kernel log one look reads. A log written faster than that is read on at the next
/// look: a job that makes the kernel talk without end does not hold the supervisor at its log.
pub const RECORDS_A_LOOK: usize = 4096;

/// Reads what the kernel log (`/dev/kmsg`, opened so that a read never waits) has now, a record a read, and
/// hands each of the kernel's own messages to `each`. It ends at the log's end, at an error, or after
/// [`RECORDS_A_LOOK`] records.
pub fn read_records(log: &mut impl std::io::Read, mut each: impl FnMut(&str)) {
    let mut record = [0u8; 8192];
    for _ in 0..RECORDS_A_LOOK {
        match log.read(&mut record) {
            Ok(read @ 1..) => {
                if let Some(message) = message_of(&String::from_utf8_lossy(&record[..read])) {
                    each(message);
                }
            }
            // The log dropped records we were too slow for; the next read goes on after them.
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            _ => break,
        }
    }
}

/// How often the kernel has stopped a process of a control group, from its `memory.events`.
pub fn kills_in_events(memory_events: &str) -> u64 {
    memory_events
        .lines()
        .find_map(|line| line.strip_prefix("oom_kill ")?.trim().parse().ok())
        .unwrap_or(0)
}

/// How often a control group's own limit was reached, from its `memory.events.local` (`oom`). Unlike `oom_kill`,
/// which counts each process stopped in the group or below whatever limit it ran into, this counts only the times
/// this group's limit was the one.
pub fn limit_reached(memory_events_local: &str) -> u64 {
    memory_events_local
        .lines()
        .find_map(|line| line.strip_prefix("oom ")?.trim().parse().ok())
        .unwrap_or(0)
}

/// Keeps a Docker job's containers that ran into a limit of their own (`docker run --memory`, Compose's
/// `mem_limit`) out of the job's stops: the job did not run out of memory, the container did, and its command sees
/// that as its exit code 137. A stop is the job's when the job's own limit was reached at this look or the one
/// before: the kernel counts the limit first and stops a process after, so one look may see only the first.
#[derive(Debug, Default)]
pub struct OwnLimit {
    /// How often the job's limit was reached, at the look before last and at the last.
    reached: [u64; 2],
    /// The job's `oom_kill` at the last look.
    counted: u64,
    /// The stops in it that were the containers'.
    theirs: u64,
}

impl OwnLimit {
    /// One look: the job's limit was reached `reached` times so far, and its group counts `counted` stops. Returns
    /// whether this look's stops are the job's, and the job's own count of stops to hold against its log.
    pub fn look(&mut self, reached: u64, counted: u64) -> (bool, u64) {
        let new = counted.saturating_sub(self.counted);
        self.counted = self.counted.max(counted);
        let ours = reached > self.reached[0];
        self.reached = [self.reached[1], reached];
        if !ours {
            self.theirs = self.theirs.saturating_add(new);
        }
        (ours, self.counted.saturating_sub(self.theirs))
    }
}

/// The stops the kernel counted for a job against those its log named. The log names the process and can fall
/// behind or drop records; the counter (`oom_kill` in the group's `memory.events`) is exact and names nothing.
#[derive(Debug, Default)]
pub struct Count {
    seen: u64,
    /// Looks in a row at which the counter was ahead of the log.
    behind: u32,
}

impl Count {
    /// One look: the log named `named` stops since the last one, and the counter now says `counted`. Returns
    /// whether a stop is to be reported that no log line named: when the counter was ahead for two looks in a row,
    /// since one may only be the log being late, or at once at the job's `last` look, which has no second.
    pub fn look(&mut self, named: u64, counted: u64, last: bool) -> bool {
        self.seen = self.seen.saturating_add(named);
        if counted <= self.seen {
            self.behind = 0;
            return false;
        }
        self.behind += 1;
        if self.behind < 2 && !last {
            return false;
        }
        self.seen = counted;
        self.behind = 0;
        true
    }
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
        table.truncate(LARGEST_PROCESSES);
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

/// How the kernel begins the line that names a process it stopped for memory: for a control group at its limit,
/// for the whole machine, and for the machine when it is set to stop whoever asked for the memory.
const KILLED: &[&str] = &[
    "Memory cgroup out of memory: Killed process ",
    "Out of memory: Killed process ",
    "Out of memory (oom_kill_allocating_task): Killed process ",
];

/// `Memory cgroup out of memory: Killed process 1234 (node) total-vm:...`: the id and the name. The name may
/// hold brackets. The line must begin as the kernel begins it: a job can get words of its own into the middle of
/// a kernel message (a mount option the kernel refuses is quoted in the log), never to its start.
fn parse_killed(message: &str) -> Option<(u32, String)> {
    let rest = KILLED.iter().find_map(|start| message.strip_prefix(start))?;
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

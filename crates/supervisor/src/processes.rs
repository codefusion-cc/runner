//! The job's processes as a tree, to find the ones that belong to a step.

use std::collections::BTreeMap;

/// The Actions runner's own processes, by the name the kernel has for them. What runs below them is a step.
const ACTIONS_RUNNER: &[&str] = &["run.sh", "run-helper.sh", "Runner.Listener", "Runner.Worker"];

/// One process as `/proc/<pid>/stat` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    /// Its id.
    pub pid: u32,
    /// Its parent's id.
    pub parent: u32,
    /// Its name, 15 characters at most.
    pub name: String,
    /// When it started, in clock ticks since boot: with the id it names one process for good.
    pub started: u64,
}

/// Reads one `/proc/<pid>/stat`. The name sits in brackets and may itself hold brackets and spaces, so the last
/// closing bracket ends it.
pub fn parse_stat(text: &str) -> Option<Process> {
    let open = text.find('(')?;
    let close = text.rfind(')')?;
    if close < open {
        return None;
    }
    let pid = text[..open].trim().parse().ok()?;
    // After the name: state, parent, ..., and the start time as the 20th field.
    let mut fields = text[close + 1..].split_ascii_whitespace();
    let _state = fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let started = fields.nth(17)?.parse().ok()?;
    Some(Process {
        pid,
        parent,
        name: text[open + 1..close].to_string(),
        started,
    })
}

/// Every process at one moment, by id.
#[derive(Debug, Clone, Default)]
pub struct Table(BTreeMap<u32, Process>);

impl Table {
    /// A table of these processes.
    pub fn new(processes: impl IntoIterator<Item = Process>) -> Self {
        Self(
            processes
                .into_iter()
                .map(|process| (process.pid, process))
                .collect(),
        )
    }

    /// One process.
    pub fn get(&self, pid: u32) -> Option<&Process> {
        self.0.get(&pid)
    }

    /// The children of a process.
    pub fn children(&self, pid: u32) -> impl Iterator<Item = &Process> {
        self.0.values().filter(move |process| process.parent == pid)
    }

    /// A process and everything below it, children before their parents, so that stopping them in this order
    /// leaves no parent to restart a child.
    pub fn tree(&self, root: u32) -> Vec<u32> {
        let mut order = vec![root];
        let mut next = 0;
        // A table read while processes come and go can hold a cycle of reused ids; a process is visited once.
        while next < order.len() {
            let parent = order[next];
            next += 1;
            for child in self.children(parent) {
                if !order.contains(&child.pid) {
                    order.push(child.pid);
                }
            }
        }
        order.reverse();
        order
    }

    /// The workload's first process: the container is a launcher, the container's first process below it, and the
    /// workload below that.
    pub fn workload(&self, launcher: u32) -> Option<&Process> {
        let init = self.children(launcher).next()?;
        self.children(init.pid).next()
    }

    /// The top process of the step that `process` belongs to: its highest ancestor (or itself) that is neither the
    /// container's own nor the Actions runner's. `None` when the process is not in the container, or is one of
    /// those: then no step lost a process and there is nothing to stop.
    pub fn step_of(&self, process: u32, launcher: u32) -> Option<&Process> {
        // From the process up to the launcher.
        let mut path = vec![self.get(process)?];
        while path.last()?.pid != launcher {
            let parent = self.get(path.last()?.parent)?;
            if path.len() > self.0.len() {
                return None;
            }
            path.push(parent);
        }
        // From the top down: the launcher, the container's first process, the workload, then the Actions
        // runner's chain.
        path.reverse();
        path.into_iter()
            .skip(3)
            .find(|process| !ACTIONS_RUNNER.contains(&process.name.as_str()))
    }
}

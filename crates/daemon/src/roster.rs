use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::traits::{Slots, Ticket};

/// What a runner works on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobInfo {
    /// `owner/name`.
    pub repo: String,
    /// The job's name.
    pub name: String,
    /// The workflow file at its ref.
    pub workflow: String,
    /// The workflow run.
    pub run_id: i64,
    /// When the job started, by our clock.
    pub started: SystemTime,
}

/// One runner of this machine: a registered Actions runner and the machine it runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    /// The Actions runner's name.
    pub name: String,
    /// The host's handle for its machine.
    pub handle: String,
    /// The service's ID for the registration.
    pub runner_id: i64,
    /// The scale set it belongs to.
    pub scale_set_id: i64,
    /// The slot it holds until its machine is gone.
    pub ticket: Option<Ticket>,
    /// When it started.
    pub started: SystemTime,
    /// Set once the service reports the runner took a job.
    pub busy: bool,
    /// Already running when the Runner started, so whether it has a job is unknown: never scaled down.
    pub adopted: bool,
    /// When its job completed; its machine goes away on its own shortly after.
    pub done: Option<SystemTime>,
    /// The job it took.
    pub job: Option<JobInfo>,
}

/// The runners of this machine, for every scale set. Safe to share between the scalers and whoever watches machines
/// come and go.
#[derive(Debug, Default)]
pub struct Roster {
    runners: Mutex<BTreeMap<String, Runner>>,
}

impl Roster {
    /// An empty roster.
    pub fn new() -> Roster {
        Roster::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Runner>> {
        self.runners.lock().expect("roster")
    }

    /// Records a started runner.
    pub fn add(&self, runner: Runner) {
        self.lock().insert(runner.name.clone(), runner);
    }

    /// Records a machine found running at start, unless the roster knows it already.
    pub fn adopt(&self, mut runner: Runner) {
        runner.adopted = true;
        self.lock().entry(runner.name.clone()).or_insert(runner);
    }

    /// A runner by name.
    pub fn get(&self, name: &str) -> Option<Runner> {
        self.lock().get(name).cloned()
    }

    /// Forgets a runner and returns it.
    pub fn remove(&self, name: &str) -> Option<Runner> {
        self.lock().remove(name)
    }

    /// Records that a runner took a job. Runners this machine does not know are ignored.
    pub fn mark_busy(&self, name: &str, job: JobInfo) {
        if let Some(r) = self.lock().get_mut(name) {
            r.busy = true;
            r.job = Some(job);
        }
    }

    /// Records that a runner's job completed. Runners this machine does not know are ignored.
    pub fn mark_done(&self, name: &str, at: SystemTime) {
        if let Some(r) = self.lock().get_mut(name) {
            r.done.get_or_insert(at);
        }
    }

    /// Every runner of a scale set (all of them for 0), by name.
    pub fn runners(&self, scale_set_id: i64) -> Vec<Runner> {
        self.lock()
            .values()
            .filter(|r| scale_set_id == 0 || r.scale_set_id == scale_set_id)
            .cloned()
            .collect()
    }

    /// How many runners of a scale set have a job to do or wait for one.
    pub fn active(&self, scale_set_id: i64) -> usize {
        self.lock()
            .values()
            .filter(|r| r.scale_set_id == scale_set_id && r.done.is_none())
            .count()
    }

    /// A runner's machine is gone: forgets the runner and gives its slot back, once.
    pub fn exited(&self, name: &str, slots: &dyn Slots) -> Option<Runner> {
        let runner = self.remove(name)?;
        if let Some(ticket) = runner.ticket {
            slots.release(ticket);
        }
        Some(runner)
    }
}

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::SystemTime;

use crate::traits::{Slots, Ticket};

/// What an Actions runner works on.
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

/// One Actions runner of this machine: a registered Actions runner and the machine it runs in.
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
    /// Set once the service reports the Actions runner took a job.
    pub busy: bool,
    /// Already running when the Runner started, so whether it has a job is unknown: never scaled down.
    pub adopted: bool,
    /// When its job completed; its machine goes away on its own shortly after.
    pub done: Option<SystemTime>,
    /// The job it took.
    pub job: Option<JobInfo>,
}

/// The Actions runners of this machine, for every scale set. Safe to share between the scalers and whoever watches
/// machines come and go.
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

    /// Records a started Actions runner.
    pub fn add(&self, runner: Runner) {
        self.lock().insert(runner.name.clone(), runner);
    }

    /// Records a machine found running at start, unless the roster knows it already.
    pub fn adopt(&self, mut runner: Runner) {
        runner.adopted = true;
        self.lock().entry(runner.name.clone()).or_insert(runner);
    }

    /// An Actions runner by name.
    pub fn get(&self, name: &str) -> Option<Runner> {
        self.lock().get(name).cloned()
    }

    /// Forgets an Actions runner and returns it.
    pub fn remove(&self, name: &str) -> Option<Runner> {
        self.lock().remove(name)
    }

    /// Records that an Actions runner of a scale set took a job. A name this machine does not know is ignored, and so
    /// is one of another scale set: whoever can register Actions runners there chooses their names.
    pub fn mark_busy(&self, scale_set_id: i64, name: &str, job: JobInfo) {
        if let Some(r) = self.lock().get_mut(name)
            && r.scale_set_id == scale_set_id
        {
            r.busy = true;
            r.job = Some(job);
        }
    }

    /// Records that the job of a scale set's Actions runner completed, and says whether this was news: `false` for an
    /// Actions runner this machine does not know, for one of another scale set and for one whose job was already over.
    pub fn mark_done(&self, scale_set_id: i64, name: &str, at: SystemTime) -> bool {
        match self.lock().get_mut(name) {
            Some(r) if r.scale_set_id == scale_set_id && r.done.is_none() => {
                r.done = Some(at);
                true
            }
            _ => false,
        }
    }

    /// Every Actions runner of a scale set (all of them for 0), by name.
    pub fn runners(&self, scale_set_id: i64) -> Vec<Runner> {
        self.lock()
            .values()
            .filter(|r| scale_set_id == 0 || r.scale_set_id == scale_set_id)
            .cloned()
            .collect()
    }

    /// How many Actions runners of a scale set have a job to do or wait for one.
    pub fn active(&self, scale_set_id: i64) -> usize {
        self.lock()
            .values()
            .filter(|r| r.scale_set_id == scale_set_id && r.done.is_none())
            .count()
    }

    /// An Actions runner's machine is gone: forgets the Actions runner and gives its slot back, once.
    pub fn exited(&self, name: &str, slots: &dyn Slots) -> Option<Runner> {
        let runner = self.remove(name)?;
        if let Some(ticket) = runner.ticket {
            slots.release(ticket);
        }
        Some(runner)
    }
}

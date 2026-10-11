//! What the daemon writes down about every Actions runner it registers, so that one that starts again finds each
//! machine, each registration and each job it still has to report.

use cf_budget::Size;
use cf_lifecycle::{End, Phase, Saved};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A job of GitHub's, as its messages describe it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GithubJob {
    /// `owner/name`.
    pub repo: String,
    /// The job's name.
    pub job: String,
    /// The workflow file at its ref.
    pub workflow: String,
    /// The workflow run.
    pub run_id: i64,
    /// `push`, `pull_request` and so on.
    pub event: String,
    /// When the job was queued, in seconds since 1970.
    pub queued_at: Option<i64>,
    /// When an Actions runner got it.
    pub started_at: Option<i64>,
    /// GitHub's result, once the job completed: `succeeded`, `failed`, `canceled`.
    pub result: Option<String>,
    /// When GitHub's completion arrived here.
    pub completed_at: Option<i64>,
}

/// How a machine ended, with what was measured on the way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ending {
    /// The end, by [`crate::EndKind::name`], before GitHub's result is taken into account.
    pub kind: String,
    /// When the machine was gone, in seconds since 1970.
    pub at: i64,
    /// From the vm-host's start to the guest's `ready`, in milliseconds.
    pub boot_ms: Option<u64>,
    /// The most memory the job held.
    pub memory_peak_bytes: Option<u64>,
    /// The most the job had on its disk.
    pub disk_peak_bytes: Option<u64>,
    /// CPU time the job used, in microseconds.
    pub cpu_usec: Option<u64>,
    /// From the workload's start to its end, in milliseconds.
    pub wall_ms: Option<u64>,
    /// What the vm-host said about the end, when it said something.
    pub message: Option<String>,
}

/// One Actions runner of this Mac, from the moment its name is chosen to the moment nothing of it is left: no
/// machine, no registration at GitHub, no job to report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// The machine's name on this Mac, which names its files: the random part of the Actions runner's name.
    pub job: String,
    /// The Actions runner's name at GitHub.
    pub runner: String,
    /// GitHub's number for the registration; none while the registration is only intended.
    pub runner_id: Option<i64>,
    /// The scale set.
    pub scale_set_id: i64,
    /// `owner/name`.
    pub repo: String,
    /// The machine's size, by its label; none before a machine was asked for.
    pub size: Option<String>,
    /// The machine's memory and CPUs, and how the budget came to grant them.
    pub memory_bytes: u64,
    /// The job's own share of that memory.
    pub limit_bytes: u64,
    /// The machine's CPUs.
    pub cpus: u32,
    /// `claimed`, `assigned`, `assigned-in-use` or `adopted`.
    pub origin: String,
    /// The lifecycle's state; none while the registration is only intended.
    pub slot: Option<SavedSlot>,
    /// When the machine was asked for, in seconds since 1970.
    pub created_at: i64,
    /// The start of the Mac this was written in, by the name macOS gives it: a record from another start has no
    /// machine.
    pub boot: String,
    /// Whether the registration is known to be gone from GitHub.
    pub unregistered: bool,
    /// GitHub's job on this Actions runner, once it took one.
    pub github: Option<GithubJob>,
    /// Whether the job's start went to the console's queue.
    pub start_reported: bool,
    /// How the machine ended; none while it stands.
    pub ended: Option<Ending>,
    /// Whether the job's end went to the console's queue (or there was no job to report).
    pub end_reported: bool,
    /// Whether a process of the job was stopped for memory.
    pub oom: bool,
}

impl Record {
    /// The machine's size.
    pub fn size(&self) -> Option<Size> {
        self.size.as_deref().and_then(Size::from_label)
    }

    /// Whether nothing is left to do for this Actions runner.
    pub fn finished(&self) -> bool {
        self.unregistered && self.end_reported && (self.ended.is_some() || self.slot.is_none())
    }
}

/// [`cf_lifecycle::Saved`] as it is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedSlot {
    /// The phase, by number: see [`SavedSlot::phase`].
    pub phase: u8,
    /// How many machines were started.
    pub starts: u32,
    /// The decided end, by number.
    pub end: Option<u8>,
    /// Whether a process of the job was stopped for memory.
    pub oom: bool,
    /// Whether a job started.
    pub ran_job: bool,
}

const PHASES: [Phase; 9] = [
    Phase::Reserved,
    Phase::Starting,
    Phase::Retrying,
    Phase::Idle,
    Phase::Running,
    Phase::Finishing,
    Phase::Stopping,
    Phase::Killing,
    Phase::Released,
];

const ENDS: [End; 11] = [
    End::Ok,
    End::Oom,
    End::EvictedMemory,
    End::EvictedDisk,
    End::Timeout,
    End::GuestLost,
    End::HostSlept,
    End::Stopped,
    End::Lost,
    End::StartFailed,
    End::NoJob,
];

impl From<Saved> for SavedSlot {
    fn from(saved: Saved) -> SavedSlot {
        let index = |found: Option<usize>| u8::try_from(found.unwrap_or(0)).unwrap_or(0);
        SavedSlot {
            phase: index(PHASES.iter().position(|p| *p == saved.phase)),
            starts: saved.starts,
            end: saved.end.map(|end| index(ENDS.iter().position(|e| *e == end))),
            oom: saved.oom,
            ran_job: saved.ran_job,
        }
    }
}

impl SavedSlot {
    /// The lifecycle's own form. A number this version does not know reads as the phase or end that is safest to
    /// meet: a machine being killed, lost by its guest.
    pub fn saved(&self) -> Saved {
        Saved {
            phase: PHASES
                .get(usize::from(self.phase))
                .copied()
                .unwrap_or(Phase::Killing),
            starts: self.starts,
            end: self
                .end
                .map(|end| ENDS.get(usize::from(end)).copied().unwrap_or(End::GuestLost)),
            oom: self.oom,
            ran_job: self.ran_job,
        }
    }
}

/// Where records are kept. A failed write is logged and the daemon goes on: what it knows in memory still holds
/// until it ends, and the next write tries again.
pub trait Store: Send + Sync {
    /// Writes a record, over the one of the same job.
    fn save(&self, record: &Record);
    /// Removes a job's record.
    fn remove(&self, job: &str);
    /// Every record, in the order the jobs were made.
    fn load(&self) -> Vec<Record>;
}

/// Records in memory, for tests: what a daemon that "starts again" in a test finds is what the one before wrote.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore(Arc<Mutex<BTreeMap<String, Record>>>);

impl Store for MemoryStore {
    fn save(&self, record: &Record) {
        self.0
            .lock()
            .expect("records")
            .insert(record.job.clone(), record.clone());
    }

    fn remove(&self, job: &str) {
        self.0.lock().expect("records").remove(job);
    }

    fn load(&self) -> Vec<Record> {
        let mut records: Vec<Record> = self.0.lock().expect("records").values().cloned().collect();
        records.sort_by(|a, b| (a.created_at, &a.job).cmp(&(b.created_at, &b.job)));
        records
    }
}

/// Records as files, `<folder>/<job>.json`, each replaced in one step.
#[derive(Debug, Clone)]
pub struct DiskStore(PathBuf);

impl DiskStore {
    /// The records in this folder, which is made for this user alone when it is not there.
    pub fn new(dir: impl AsRef<Path>) -> std::io::Result<DiskStore> {
        let dir = dir.as_ref().to_path_buf();
        cf_secret::private_dir(&dir)?;
        Ok(DiskStore(dir))
    }
}

use cf_secret::replace_private;

impl Store for DiskStore {
    fn save(&self, record: &Record) {
        let path = self.0.join(format!("{}.json", record.job));
        let written = serde_json::to_vec(record)
            .map_err(std::io::Error::other)
            .and_then(|json| replace_private(&path, &json));
        if let Err(error) = written {
            tracing::error!(job = %record.job, %error, "could not write a job's record");
        }
    }

    fn remove(&self, job: &str) {
        if let Err(error) = std::fs::remove_file(self.0.join(format!("{job}.json")))
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::error!(job = %job, %error, "could not remove a job's record");
        }
    }

    fn load(&self) -> Vec<Record> {
        let mut records: Vec<Record> = std::fs::read_dir(&self.0)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|e| e == "json"))
            .filter_map(|entry| {
                let text = std::fs::read(entry.path()).ok()?;
                match serde_json::from_slice::<Record>(&text) {
                    Ok(record) => Some(record),
                    Err(error) => {
                        // A record nobody can read names a machine by its file alone: the machine is ended as one
                        // without a record, and the file goes.
                        tracing::error!(file = %entry.path().display(), %error, "a job's record is unreadable; removed");
                        let _ = std::fs::remove_file(entry.path());
                        None
                    }
                }
            })
            .collect();
        records.sort_by(|a, b| (a.created_at, &a.job).cmp(&(b.created_at, &b.job)));
        records
    }
}

//! What the daemon writes down about every Actions runner it registers, so that one that starts again finds each
//! machine, each registration and each job it still has to report.

use cf_budget::Size;
use cf_lifecycle::{End, Event, Limits, Phase, Saved, Slot};

use crate::EndKind;
use crate::docker_check::DockerCheck;
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
    /// No message of GitHub's named this job: its answers said a job ran (busy, refused for a job, or the registration
    /// gone by itself under a VM that came up), not which.
    #[serde(default)]
    pub inferred: bool,
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

impl Ending {
    /// A job ran on the machine after all: an end without a job was the Runner's stop. Says whether it was.
    pub(crate) fn job_ran(&mut self) -> bool {
        let was = self.kind == EndKind::NoJob.name();
        if was {
            self.kind = EndKind::Stopped.name().to_string();
        }
        was
    }
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
    /// Whether the machine has a Docker daemon (`cf-docker`): it counts against the Mac's Docker cap.
    #[serde(default)]
    pub docker: bool,
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
    /// The rules the record was written under: [`RULES`]. A record of an older Runner is read once, by
    /// [`Record::upgrade`], and written under these rules from then on.
    #[serde(default)]
    pub rules: u32,
    /// A test run's ([`crate::test_run`]): a machine with no Actions runner and no registration at GitHub, so
    /// `unregistered` from the start and no word of GitHub's ever. None for an Actions runner's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test_run: Option<TestRunFacts>,
    /// The Docker check's machine ([`crate::docker_check`]): no Actions runner, no registration at GitHub, no job
    /// for the console; its end is the check's answer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub probe: bool,
}

/// What a test run's record keeps beside an Actions runner's: what its job events name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRunFacts {
    /// What the developer calls the run, the job's name in its events.
    pub label: String,
    /// When its machine said it is up, in seconds since 1970: the job's start.
    pub started_at: Option<i64>,
}

/// The rules every record is written under now.
///
/// 2: what the Runner knows of a machine is stored as facts, never inferred: whether its VM came up
/// ([`Saved::up`]), whether GitHub said its job is over before the VM was up ([`Saved::completed`]), and an Actions
/// runner whose registration's removal is asked for as a phase of its own ([`Phase::Asking`]). Only a 204 to the
/// Runner's own removal says no job ran, and a job only GitHub's answers name is stored as such
/// ([`GithubJob::inferred`]). What GitHub said of the job and the lifecycle agree at every write
/// ([`Record::agrees`]).
///
/// 1 (earlier builds of this branch): the same evidence, with the removal asked for written as an idle slot that
/// expects [`End::NoJob`], and no stored `up`. 0 (main): `unregistered`, a settled end and a stop for no job could
/// stand for a belief.
pub const RULES: u32 = 2;

/// What restore makes of a record ([`Record::upgrade`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upgrade {
    /// Written under these rules: it goes on as it is.
    Current,
    /// Written by an older Runner: read once under these rules, and written so.
    Upgraded,
}

/// A record written under these rules whose facts disagree ([`Record::agrees`]): a mistake of the Runner's, which
/// restore says aloud and does not read as an older Runner's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("the record of {} was written under these rules and its facts disagree", .0.runner)]
pub struct Disagrees(pub Box<Record>);

impl Record {
    /// The machine's size.
    pub fn size(&self) -> Option<Size> {
        self.size.as_deref().and_then(Size::from_label)
    }

    /// Whether it belongs to this repository's scale set of this number, as [`crate::Runner::is_of`] says.
    pub fn is_of(&self, repo: &str, scale_set_id: i64) -> bool {
        self.scale_set_id == scale_set_id && self.repo.eq_ignore_ascii_case(repo)
    }

    /// Whether this machine's VM came up, so that its Actions runner may have taken a job.
    pub fn up(&self) -> bool {
        self.slot.is_some_and(|slot| slot.saved().up)
    }

    /// Whether a job ran is GitHub's to say: the VM came up, nobody heard of a job, and the registration is
    /// still there. The answer to its removal says it ([`crate::Answer`]).
    pub fn unheard(&self) -> bool {
        self.github.is_none() && !self.unregistered && self.up()
    }

    /// Whether the machine is the Runner's own, with no Actions runner and nothing at GitHub: a test run's or the
    /// Docker check's. Its job starts when its guest is up and ends with the machine.
    pub fn local(&self) -> bool {
        self.test_run.is_some() || self.probe
    }

    /// Whether nothing is left to do for this Actions runner.
    pub fn finished(&self) -> bool {
        self.unregistered && self.end_reported && (self.ended.is_some() || self.slot.is_none())
    }

    /// Whether the facts of this record agree, as every write must leave them:
    ///
    /// - a registration with no machine has no job and no end;
    /// - the machine's end is written exactly when its slot is over;
    /// - a slot that waits for, asks about, runs or finishes a job has a VM that came up;
    /// - one asking for its registration's removal has heard of no job and still has the registration;
    /// - an end without a job stands only beside GitHub's 204 (the registration gone) and no job;
    /// - a job GitHub named ran, on a slot that no longer waits for one or boots, and none runs a job GitHub said
    ///   is over.
    pub fn agrees(&self) -> bool {
        let Some(slot) = self.slot else {
            return self.github.is_none() && self.ended.is_none();
        };
        let saved = slot.saved();
        let waits_or_works = matches!(
            saved.phase,
            Phase::Idle | Phase::Asking | Phase::Running | Phase::Finishing
        );
        let no_job = saved.end == Some(End::NoJob);
        let job_agrees = self.github.as_ref().is_none_or(|github| {
            saved.ran_job
                && !matches!(
                    saved.phase,
                    Phase::Starting | Phase::Retrying | Phase::Idle | Phase::Asking
                )
                && !(github.result.is_some() && saved.phase == Phase::Running)
        });
        self.ended.is_some() == (saved.phase == Phase::Released)
            && (!waits_or_works || saved.up)
            && (saved.phase != Phase::Asking || (self.github.is_none() && !self.unregistered))
            && (!no_job || (self.github.is_none() && !saved.ran_job && self.unregistered))
            && job_agrees
    }

    /// Reads a record as these rules ([`RULES`]) would have written it: restore's one place for what an older Runner
    /// left (`docs/job-state.md`). A record of these rules is taken as it is, and one whose facts disagree is an
    /// error, never read as an older Runner's.
    ///
    /// An older record is changed in two steps and no more:
    ///
    /// 1. **Its beliefs are cleared.** What it lacks as a fact (whether the VM came up) is read once from what it
    ///    holds. Without GitHub's word of a job, main's `unregistered`, settled end and stop for no job of a VM that
    ///    came up may each stand for a belief: they are dropped, so GitHub's answer to the removal decides, and a
    ///    VM that stands asks for it rather than being stopped. An end without a job beside a job, or without the
    ///    registration gone, is a stop.
    /// 2. **GitHub's stored word is replayed through the lifecycle** ([`Slot::hear`]), as if heard now: its start,
    ///    its end, and the registration gone under a machine that stands (under a job GitHub named, that job over).
    pub fn upgrade(mut self) -> Result<(Record, Upgrade), Disagrees> {
        if self.rules >= RULES {
            return if self.agrees() {
                Ok((self, Upgrade::Current))
            } else {
                Err(Disagrees(Box::new(self)))
            };
        }
        // Under rules 1 `unregistered` and an end without a job were written only on GitHub's answer.
        let trusted = self.rules >= 1;
        self.rules = RULES;
        let Some(slot) = self.slot else {
            return Ok((self, Upgrade::Upgraded));
        };
        let mut saved = slot.saved();
        let gone = self.ended.is_some();

        // 1. Facts it lacks, and beliefs it held.
        saved.up = saved.ran_job
            || self.github.is_some()
            || self.ended.as_ref().is_some_and(|e| e.boot_ms.is_some())
            || matches!(saved.phase, Phase::Idle | Phase::Running | Phase::Finishing)
            || (matches!(saved.phase, Phase::Stopping | Phase::Killing | Phase::Released)
                && !matches!(saved.end, Some(End::StartFailed | End::Lost)));
        saved.completed = false;
        saved.ran_job |= matches!(saved.phase, Phase::Running | Phase::Finishing);
        if gone {
            saved.phase = Phase::Released;
        } else if saved.phase == Phase::Released {
            // Over, with its end never written: ended again at restore.
            saved.phase = Phase::Killing;
        }
        if saved.phase == Phase::Idle && saved.end == Some(End::NoJob) {
            saved.phase = Phase::Asking;
            saved.end = None;
        }
        if !trusted && self.github.is_none() {
            let stopped_for_no_job = matches!(saved.phase, Phase::Stopping | Phase::Killing)
                && matches!(saved.end, Some(End::NoJob | End::Stopped));
            let believed = self.unregistered || stopped_for_no_job || saved.end == Some(End::NoJob);
            if !gone {
                // Its VM may be up even where the record says it boots: a crash can come before that write.
                self.unregistered = false;
                if believed
                    && saved.up
                    && matches!(saved.phase, Phase::Idle | Phase::Stopping | Phase::Killing)
                {
                    saved.phase = Phase::Asking;
                    saved.end = None;
                }
            } else if saved.up {
                self.unregistered = false;
                self.end_reported = false;
            }
        }
        let not_no_job = self.github.is_some() || saved.ran_job || !self.unregistered;
        if not_no_job {
            if saved.end == Some(End::NoJob) {
                saved.end = Some(End::Stopped);
            }
            if let Some(ending) = self.ended.as_mut() {
                ending.job_ran();
            }
        }

        // 2. GitHub's word, as the lifecycle takes it.
        let mut words = Vec::new();
        if let Some(github) = &self.github {
            words.push(Event::JobStarted);
            if github.result.is_some() {
                words.push(Event::JobCompleted);
            }
        }
        if self.unregistered && !gone && saved.phase != Phase::Reserved {
            // Gone under a job GitHub named: a one-job Actions runner removes its own once its job is over, as
            // `Machines::answered` reads a 404.
            match &self.github {
                None => words.push(Event::Unregistered),
                Some(github) if github.result.is_none() => words.push(Event::JobCompleted),
                Some(_) => {}
            }
        }
        self.slot = Some(Slot::hear(Limits::default(), saved, &words).into());
        Ok((self, Upgrade::Upgraded))
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
    /// Whether the VM came up.
    #[serde(default)]
    pub up: bool,
    /// Whether GitHub said the job is over before the VM was up.
    #[serde(default)]
    pub completed: bool,
}

const PHASES: [Phase; 10] = [
    Phase::Reserved,
    Phase::Starting,
    Phase::Retrying,
    Phase::Idle,
    Phase::Running,
    Phase::Finishing,
    Phase::Stopping,
    Phase::Killing,
    Phase::Released,
    Phase::Asking,
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
            up: saved.up,
            completed: saved.completed,
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
            up: self.up,
            completed: self.completed,
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
    /// The last Docker check that failed ([`crate::docker_check`]), kept across starts so that nothing Docker comes
    /// to a host whose machines start no Docker daemon until a check passes; none when there is none or it cannot be
    /// read.
    fn docker_failure(&self) -> Option<DockerCheck>;
    /// Keeps a failed check, or forgets the one kept (`None`, once a check passed).
    fn keep_docker_failure(&self, check: Option<&DockerCheck>);
}

/// Records in memory, for tests: what a daemon that "starts again" in a test finds is what the one before wrote.
#[derive(Debug, Clone, Default)]
pub struct MemoryStore(
    Arc<Mutex<BTreeMap<String, Record>>>,
    Arc<Mutex<Option<DockerCheck>>>,
);

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

    fn docker_failure(&self) -> Option<DockerCheck> {
        self.1.lock().expect("docker").clone()
    }

    fn keep_docker_failure(&self, check: Option<&DockerCheck>) {
        *self.1.lock().expect("docker") = check.cloned();
    }
}

/// The file beside the records that keeps a failed Docker check: not `.json`, so no record is read from it.
const DOCKER_FAILURE: &str = "docker-check.failed";

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

    fn docker_failure(&self) -> Option<DockerCheck> {
        let path = self.0.join(DOCKER_FAILURE);
        let read = match std::fs::read(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            read => read.map_err(|error| error.to_string()),
        };
        // Only a failure is ever written: anything else in the file is no answer.
        match read.and_then(|bytes| {
            serde_json::from_slice::<DockerCheck>(&bytes).map_err(|error| error.to_string())
        }) {
            Ok(check) if !check.ok => Some(check),
            Ok(_) => {
                tracing::warn!(file = %path.display(), "the kept Docker check is no failure; Docker is unknown");
                None
            }
            Err(error) => {
                tracing::warn!(file = %path.display(), %error, "the kept Docker check cannot be read; Docker is unknown");
                None
            }
        }
    }

    fn keep_docker_failure(&self, check: Option<&DockerCheck>) {
        let path = self.0.join(DOCKER_FAILURE);
        let kept = match check {
            Some(check) => serde_json::to_vec(check)
                .map_err(std::io::Error::other)
                .and_then(|json| replace_private(&path, &json)),
            None => match std::fs::remove_file(&path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                removed => removed,
            },
        };
        if let Err(error) = kept {
            tracing::error!(%error, "could not keep the Docker check's failure");
        }
    }
}

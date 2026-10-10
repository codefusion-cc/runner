use cf_scaleset::{Error, JitConfig};
use cf_secret::Secret;
use std::future::Future;
use time::OffsetDateTime;

/// A slot that was promised to a job or given to a runner. The budget's `SlotId` in the real daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Ticket(pub u64);

/// Why no slot was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// The machine's slots, which every scale set together share: the budget, from the scaler's side.
///
/// A slot is promised before a job is acquired ([`Slots::reserve`]), made a runner's when the service says the job is
/// assigned ([`Slots::claim`]), and given back once, when its virtual machine is gone ([`Slots::release`]); never
/// while the machine may still run. Reservations that nobody claims run out on their own, so a job cancelled between
/// the offer and the assignment does not hold a slot for long.
pub trait Slots: Send + Sync {
    /// Promises a slot of this size to a job about to be acquired. A refusal says why: the machine takes no new
    /// jobs now (someone works on it, memory is short), or it is full.
    fn reserve(&self, size_gb: u32) -> Result<Ticket, Refusal>;

    /// Gives a promise back, or a claimed slot whose runner did not start.
    fn release(&self, ticket: Ticket);

    /// A slot for the runner of a job already assigned: the promised one, or, when the promise ran out or there was
    /// none, a free one even while the machine is closed to new jobs, since the job is already this machine's.
    /// `None` when the machine has no room.
    fn claim(&self, size_gb: u32, promised: Option<Ticket>) -> Option<Ticket>;

    /// How many more slots of this size would open now; 0 while the machine takes no new jobs.
    fn open_slots(&self, size_gb: u32) -> usize;

    /// Whether the machine takes new jobs, and why not.
    fn state(&self) -> (bool, String);
}

/// What a runner's machine is started with.
#[derive(Debug)]
pub struct RunnerSpec {
    /// The Actions runner's name.
    pub name: String,
    /// The repository it serves, `owner/name`.
    pub repo: String,
    /// The scale set.
    pub scale_set_id: i64,
    /// The service's ID for the registered runner.
    pub runner_id: i64,
    /// The memory size class in GB.
    pub size_gb: u32,
    /// The slot it runs in.
    pub ticket: Ticket,
    /// What the Actions runner starts with. A credential.
    pub jit_config: Secret<String>,
}

/// A host could not start, stop or ask about a runner's machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);

/// Starts and stops the virtual machines of runners: the VM monitor and the lifecycle, from the scaler's side.
pub trait Hosts: Send + Sync {
    /// Starts a machine for the runner and returns its handle.
    fn start(&self, spec: RunnerSpec) -> impl Future<Output = Result<String, HostError>> + Send;

    /// Stops a machine and removes it.
    fn stop(&self, handle: &str) -> impl Future<Output = Result<(), HostError>> + Send;

    /// Whether the machine lost a process because it had used up its memory.
    fn out_of_memory(&self, handle: &str) -> impl Future<Output = Result<bool, HostError>> + Send;
}

/// Acquires jobs for the scale set (the message session).
pub trait Jobs: Send + Sync {
    /// Acquires jobs by request ID and returns the ones this scale set got.
    fn acquire(&self, request_ids: &[i64]) -> impl Future<Output = Result<Vec<i64>, Error>> + Send;
}

/// Registers and removes the scale set's runners (the scale set client).
pub trait Registry: Send + Sync {
    /// Registers a runner that takes one job.
    fn generate_jit(&self, name: &str) -> impl Future<Output = Result<JitConfig, Error>> + Send;

    /// Removes a registered runner; one with a job refuses.
    fn remove_runner(&self, runner_id: i64) -> impl Future<Output = Result<(), Error>> + Send;
}

/// Which kind of job event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// A runner took a job.
    Started,
    /// A job is over.
    Completed,
}

/// A job message as the console records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    /// Started or completed.
    pub kind: EventKind,
    /// The Actions runner; empty for a job cancelled before any runner had it.
    pub runner: String,
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
    /// When the job was queued.
    pub queued_at: Option<OffsetDateTime>,
    /// When a runner got it.
    pub started_at: Option<OffsetDateTime>,
    /// When this happened, by our clock.
    pub at: OffsetDateTime,
    /// The result of a completed job.
    pub result: String,
    /// Whether the job lost a process for memory.
    pub out_of_memory: bool,
}

/// Hears of jobs on the scale set's runners.
pub trait Events: Send + Sync {
    /// A job started or completed on a runner of this machine.
    fn job(&self, event: &JobEvent);

    /// A completed job had used up its memory.
    fn out_of_memory(&self, event: &JobEvent);
}

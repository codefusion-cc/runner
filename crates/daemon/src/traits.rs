use cf_budget::{Size, SlotId};
use cf_scaleset::{Error, JitConfig};
use cf_secret::Secret;
use cf_vmhost::OutOfMemory;
use std::future::Future;
use time::OffsetDateTime;

/// A slot that was promised to a job or given to an Actions runner: the budget's slot.
pub type Ticket = SlotId;

/// Why no slot was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal(pub String);

/// What a job that is already this machine's gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// A slot: start the Actions runner now.
    Start(Ticket),
    /// A place in line. The job starts when [`Slots::ready`] says so; a job that went away gives its place back
    /// with [`Slots::withdraw`].
    Waits {
        /// The place in line.
        ticket: Ticket,
        /// What it waits for, in the budget's word.
        why: String,
    },
    /// No change of memory, of who is at the Mac or of what else runs makes this size fit: the settings do not allow
    /// it. Nothing is held.
    Never(String),
}

/// The machine's slots, which every scale set together share: the budget, from the scaler's side.
///
/// A slot is promised before a job is acquired ([`Slots::reserve`]), made an Actions runner's when the service says
/// the job is assigned ([`Slots::claim`]), and given back once: by the scaler while no machine was started for it
/// ([`Slots::release`], [`Slots::withdraw`]), by whoever watches the machines afterwards, when the machine is gone
/// and never while it may still run. The scaler gives back the promises nobody claims (the job went elsewhere, the
/// acquire failed, no assignment came within [`crate::PROMISE_TTL`], the scaler went away); the budget lets them run
/// out as well and loses nothing, since a claim then asks anew.
pub trait Slots: Send + Sync {
    /// Promises a slot of this size to a job about to be acquired. A refusal says why: the machine takes no new
    /// jobs now (someone works on it, memory is short), or it is full.
    fn reserve(&self, size: Size) -> Result<Ticket, Refusal>;

    /// Gives back a promise, or a slot whose Actions runner was not started. A ticket the budget no longer knows
    /// (given back already, run out) is not an error.
    fn release(&self, ticket: Ticket);

    /// A slot for the Actions runner of a job already assigned: the promised one, or, when the promise ran out or
    /// there was none, what the budget gives a job that is already this machine's. Such a job is never turned
    /// away for who is at the Mac: it starts, or it waits in line. The promise is used up either way.
    fn claim(&self, size: Size, promised: Option<Ticket>) -> Claim;

    /// Whether the job that waits with this ticket may start now. From then on the ticket is a slot like any other.
    fn ready(&self, ticket: Ticket) -> bool;

    /// Gives a place in line back: the job that waited went away. Also fine for a ticket that became ready and
    /// whose Actions runner was never started.
    fn withdraw(&self, ticket: Ticket);

    /// How many more slots of this size would open now; 0 while the machine takes no new jobs.
    fn open_slots(&self, size: Size) -> usize;

    /// Whether the machine takes new jobs, and why not.
    fn state(&self) -> (bool, String);
}

/// What an Actions runner's machine is started with.
#[derive(Debug)]
pub struct RunnerSpec {
    /// The Actions runner's name.
    pub name: String,
    /// The repository it serves, `owner/name`.
    pub repo: String,
    /// The scale set.
    pub scale_set_id: i64,
    /// The service's ID for the registered Actions runner.
    pub runner_id: i64,
    /// The machine's size.
    pub size: Size,
    /// The slot it runs in.
    pub ticket: Ticket,
    /// What the Actions runner starts with. A credential.
    pub jit_config: Secret<String>,
}

/// A host could not start, stop or ask about an Actions runner's machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HostError(pub String);

/// Starts and stops the virtual machines of Actions runners: the vm-hosts and the lifecycle, from the scaler's side.
pub trait Hosts: Send + Sync {
    /// An Actions runner of this name is about to be registered for a scale set. Written down before GitHub is
    /// asked, so that a Runner that dies between the registration and the machine's start finds the name again and
    /// removes the registration.
    fn registering(&self, name: &str, repo: &str, scale_set_id: i64);

    /// The registration of that name was refused or is removed again: nothing of it is left at GitHub.
    fn not_registered(&self, name: &str);

    /// GitHub gave no clear answer to the registration of that name (the connection broke, a 5xx): it may exist.
    /// Whoever removes left-over registrations looks for it by name.
    fn registration_unsure(&self, name: &str);

    /// Starts a machine for the Actions runner and returns its handle. After an error no machine stands and the
    /// slot is still the caller's: the host removed what it had begun before it answers. After a success the slot
    /// is the machine's, and goes back when the machine is gone.
    fn start(&self, spec: RunnerSpec) -> impl Future<Output = Result<String, HostError>> + Send;

    /// Asks a machine to stop; one that is gone already is stopped. The machine goes away, and its slot with it,
    /// in its own time. After an error the machine stands as before.
    fn stop(&self, handle: &str) -> impl Future<Output = Result<(), HostError>> + Send;

    /// Whether the machine lost a process because it had used up its memory. `Unknown` when nobody can say (the
    /// vm-host does not answer, the machine's state is gone), never `No`: the scaler then asks whoever watched the
    /// machine go.
    fn out_of_memory(&self, handle: &str) -> impl Future<Output = OutOfMemory> + Send;

    /// How many jobs GitHub still counts as assigned to Actions runners of this repository's scale set whose
    /// machines are gone: a job was started on each, GitHub has not said how it ended, and the registration still
    /// stands (GitHub keeps it while it counts the job), or a guest came up and went with no word from GitHub, which
    /// may count a job of it until the sweep asked. The scaler starts no machine in their place: there is no job to
    /// take.
    fn lost_with_job(&self, _repo: &str, _scale_set_id: i64) -> usize {
        0
    }
}

impl<T: Hosts> Hosts for std::sync::Arc<T> {
    fn registering(&self, name: &str, repo: &str, scale_set_id: i64) {
        (**self).registering(name, repo, scale_set_id);
    }

    fn not_registered(&self, name: &str) {
        (**self).not_registered(name);
    }

    fn registration_unsure(&self, name: &str) {
        (**self).registration_unsure(name);
    }

    fn lost_with_job(&self, repo: &str, scale_set_id: i64) -> usize {
        (**self).lost_with_job(repo, scale_set_id)
    }

    fn start(&self, spec: RunnerSpec) -> impl Future<Output = Result<String, HostError>> + Send {
        (**self).start(spec)
    }

    fn stop(&self, handle: &str) -> impl Future<Output = Result<(), HostError>> + Send {
        (**self).stop(handle)
    }

    fn out_of_memory(&self, handle: &str) -> impl Future<Output = OutOfMemory> + Send {
        (**self).out_of_memory(handle)
    }
}

/// Acquires jobs for the scale set (the message session).
pub trait Jobs: Send + Sync {
    /// Acquires jobs by request ID and returns the ones this scale set got.
    fn acquire(&self, request_ids: &[i64]) -> impl Future<Output = Result<Vec<i64>, Error>> + Send;
}

/// Registers and removes the scale set's Actions runners (the scale set client).
pub trait Registry: Send + Sync {
    /// Registers an Actions runner that takes one job.
    fn generate_jit(&self, name: &str) -> impl Future<Output = Result<JitConfig, Error>> + Send;

    /// Removes a registered Actions runner; one with a job refuses.
    fn remove_runner(&self, runner_id: i64) -> impl Future<Output = Result<(), Error>> + Send;

    /// Whether a registered Actions runner has a job, by GitHub's own record; `None` when GitHub no longer has it.
    fn runner_busy(&self, runner_id: i64) -> impl Future<Output = Result<Option<bool>, Error>> + Send;
}

/// Which kind of job event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// An Actions runner took a job.
    Started,
    /// A job is over.
    Completed,
}

/// A job message as the console records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEvent {
    /// Started or completed.
    pub kind: EventKind,
    /// The Actions runner; empty for a job cancelled before any Actions runner had it.
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
    /// When an Actions runner got it.
    pub started_at: Option<OffsetDateTime>,
    /// When this happened, by our clock.
    pub at: OffsetDateTime,
    /// The result of a completed job.
    pub result: String,
    /// Whether the job lost a process for memory.
    pub out_of_memory: bool,
}

/// Hears of jobs on the scale set's Actions runners.
pub trait Events: Send + Sync {
    /// A job started or completed on an Actions runner of this machine.
    fn job(&self, event: &JobEvent);

    /// A completed job had used up its memory.
    fn out_of_memory(&self, event: &JobEvent);
}

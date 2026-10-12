//! The messages of protocol 1, as the console reads them: camelCase on the wire, snake_case here.
//!
//! Times are RFC 3339 strings. A field the console treats as optional is left out when it is `None`.

use cf_secret::Secret;
use serde::{Deserialize, Serialize};

/// The protocol version in every request. The console refuses any other.
pub const PROTOCOL: u32 = 1;

fn is_false(value: &bool) -> bool {
    !*value
}

/// What a Mac says about itself when it trades a one-time code for its own token.
#[derive(Debug, Clone)]
pub struct Enroll {
    /// The one-time code an owner made on the console's Runners page. Never printed.
    pub code: Secret<String>,
    /// The Mac's name on the console: lower case letters and digits in groups joined by single hyphens, at most 24
    /// characters (see [`crate::is_machine_name`]).
    pub machine: String,
    /// The Mac's host name.
    pub hostname: String,
    /// The Mac's chip and model, such as `Apple M5 (Mac17,3)`.
    pub model: String,
    /// The Runner's version with the commit it was built at, such as `0.1.0+446125c0a1b2`.
    pub version: String,
    /// The Mac's CPU count, 1 to 4096.
    pub cpus: u32,
    /// The Mac's memory in bytes.
    pub memory_bytes: u64,
}

/// What the console answers to a valid code: the Mac's own token and the name the console knows it by.
#[derive(Debug, Clone)]
pub struct Enrolled {
    /// The token every later call carries. Never printed.
    pub token: Secret<String>,
    /// The Mac's name, as enrolled.
    pub name: String,
}

/// The Mac's load, memory and disk, as the budget reads them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostStatus {
    /// CPUs.
    pub cpus: u32,
    /// Load average over one minute.
    pub load1: f64,
    /// Load average over five minutes.
    pub load5: f64,
    /// Load average over fifteen minutes.
    pub load15: f64,
    /// The share of the Mac's memory that is available, 0 to 100: free memory, the file cache and purgeable memory,
    /// which is what Activity Monitor leaves when "Memory Used" is taken from the memory installed. The console
    /// shows 100 less this as memory used.
    pub free_memory_percent: f64,
    /// That available memory in bytes: what the Runner admits machines by.
    pub available_memory_bytes: u64,
    /// The available memory the Runner keeps free under the limits in force: a machine starts only when its size
    /// fits above this.
    pub memory_floor_bytes: u64,
    /// The kernel's memory pressure level, 0 to 8.
    pub memory_pressure: u32,
    /// Memory in bytes.
    pub memory_bytes: u64,
    /// Swap in use, in bytes.
    pub swap_used_bytes: u64,
    /// Swap in total, in bytes.
    pub swap_total_bytes: u64,
    /// Free disk, in bytes.
    pub disk_free_bytes: u64,
    /// Disk in total, in bytes.
    pub disk_total_bytes: u64,
    /// When the Mac booted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_time: Option<String>,
    /// Seconds since the last input, -1 when unknown.
    pub idle_seconds: f64,
}

/// A scale set of one repository and what it is doing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaleSetStatus {
    /// `owner/name`.
    pub repo: String,
    /// The scale set's name.
    pub name: String,
    /// GitHub's number for it.
    pub id: i64,
    /// The labels it answers to, at most 30.
    pub labels: Vec<String>,
    /// How many Actions runners it may have.
    pub capacity: u32,
    /// Jobs waiting for one.
    pub waiting: u32,
    /// What went wrong last, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When that happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_at: Option<String>,
}

/// The job an Actions runner has.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerJob {
    /// `owner/name`.
    pub repo: String,
    /// The job's name.
    pub name: String,
    /// The workflow.
    pub workflow: String,
    /// The workflow run.
    pub run_id: u64,
    /// When the job started.
    pub started: String,
}

/// What an Actions runner's machine uses.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerUsage {
    /// CPU in percent, -1 when unknown.
    pub cpu_percent: f64,
    /// Memory in bytes.
    pub memory_bytes: u64,
}

/// An Actions runner of this Mac and its machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerStatus {
    /// The Actions runner's name at GitHub, at most 400 characters (the console keeps 100).
    pub name: String,
    /// The scale set it belongs to.
    pub scale_set: i64,
    /// `idle`, `busy` and so on, at most 20 characters kept.
    pub state: String,
    /// When it came up.
    pub started: String,
    /// Its job, while it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<RunnerJob>,
    /// What its machine uses, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<RunnerUsage>,
}

/// When and how the Runner looks for a new version of itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Update {
    /// Where updates come from.
    pub source: String,
    /// The source before a move, if there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub moved_from: Option<String>,
    /// When it last looked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// What went wrong last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// When that happened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_at: Option<String>,
    /// Failures in a row.
    pub failures: u32,
}

/// What the Mac does now. The console keeps the newest and not the history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    /// The Mac's name, as enrolled. A different one renames the Mac on the console, unless another has the name.
    pub machine: String,
    /// The Runner's version with the commit it was built at, such as `0.1.0+446125c0a1b2`.
    pub version: String,
    /// When this status was made.
    pub updated_at: String,
    /// Whether the Mac takes jobs.
    pub open: bool,
    /// Why it does not, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// How many jobs at most.
    pub max_jobs: u32,
    /// Whether the console's pause is in effect on the Mac.
    pub paused: bool,
    /// The Mac's readings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<HostStatus>,
    /// Why there are none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_error: Option<String>,
    /// What went wrong finding repositories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery_error: Option<String>,
    /// The Runner's updates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update: Option<Update>,
    /// Why the Mac serves no repository, when it does not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_serving: Option<String>,
    /// For how many seconds jobs the Mac serves have waited while a machine of their size fits on it, or while it
    /// runs nothing at all; 0 (and left out) while none wait so. A few seconds are normal: GitHub hands a job over
    /// within a poll. Over a minute the Mac is idle while work waits for it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub idle_with_jobs_waiting_seconds: u64,
    /// What the Runner believes keeps those jobs from starting, while they wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_with_jobs_waiting_reason: Option<String>,
    /// Its scale sets, at most 500.
    pub scale_sets: Vec<ScaleSetStatus>,
    /// Its Actions runners, at most 64.
    pub runners: Vec<RunnerStatus>,
}

/// A power source reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Power {
    /// Whether the Mac runs on its battery.
    pub on_battery: bool,
    /// The battery's charge, 0 to 100.
    pub percent: u32,
    /// Whether it is charging.
    pub charging: bool,
}

/// What the Mac as a whole is doing, for the console's chart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// CPU in percent, -1 when unknown.
    pub cpu_percent: f64,
    /// The power source, when there is a battery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<Power>,
}

/// The sizes of a Mac's virtual machines.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmSettings {
    /// CPUs.
    pub cpus: u32,
    /// Memory in GB.
    pub memory_gb: u32,
    /// Disk in GB.
    pub disk_gb: u32,
}

/// The settings the Mac runs with, which the console can change with a `settings` command.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    /// How many jobs at most.
    pub max_jobs: u32,
    /// Minutes an idle slot is kept.
    pub idle_minutes: u32,
    /// The load per CPU above which no job starts.
    pub max_load_per_cpu: f64,
    /// The free memory below which no job starts, in percent.
    pub min_free_memory_percent: f64,
    /// Memory a job gets, in GB.
    pub job_memory_gb: f64,
    /// CPUs a job gets, 0 for the default.
    pub job_cpus: f64,
    /// The longest a job may run, in hours.
    pub max_job_hours: f64,
    /// The machine sizes.
    pub vm: VmSettings,
    /// Whether the Runner updates itself.
    pub auto_update: bool,
    /// Where updates come from.
    pub source: String,
    /// The branch they come from.
    pub source_branch: String,
    /// Repositories served, at most 200.
    pub repos: Vec<String>,
    /// Public repositories served, at most 200.
    pub public_repos: Vec<String>,
}

/// The guest image in use.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Image {
    /// The commit it was built from.
    pub commit: String,
    /// When it was built.
    pub built: String,
}

/// The kinds of job event the console reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobKind {
    /// An Actions runner got the job.
    Started,
    /// The job is over.
    Completed,
}

/// How a job's machine was sized and what it used. The console does not read this yet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MachineInfo {
    /// The size's label, such as `cf-4g`.
    pub size: String,
    /// The machine's memory in bytes.
    pub memory_bytes: u64,
    /// The job's own share of it, in bytes.
    pub limit_bytes: u64,
    /// The machine's CPUs.
    pub cpus: u32,
    /// How the budget came to grant the slot: `claimed`, `assigned`, `assigned-in-use` or `adopted`.
    pub origin: String,
    /// Milliseconds from the start to the guest's `ready`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boot_ms: Option<u64>,
    /// The most memory the job held, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_peak_bytes: Option<u64>,
    /// The most disk the job held, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_peak_bytes: Option<u64>,
    /// CPU time the job used, in microseconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_usec: Option<u64>,
    /// Milliseconds from the workload's start to its end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_ms: Option<u64>,
}

/// One thing that happened to a job, kept until the console has it.
///
/// The `id` names the event: the same event sent twice has the same id. `end` and `machine` are the Runner's own;
/// the console drops them today.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JobEvent {
    /// The event's name, at most 128 characters. Unique per event, the same on every resend.
    #[serde(default)]
    pub id: String,
    /// Started or completed.
    pub kind: JobKind,
    /// The Actions runner's name at GitHub, 1 to 100 characters.
    pub runner: String,
    /// `owner/name`, at most 200 characters kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// The job's name, at most 300 characters kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
    /// The workflow, at most 400 characters kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The workflow run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    /// `push`, `pull_request` and so on, at most 50 characters kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    /// When the job was queued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_at: Option<String>,
    /// When an Actions runner got it; on a completion, when its start event never came.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// When it happened.
    pub at: String,
    /// GitHub's result on a completion, at most 30 characters kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Whether the job used up its memory so that the kernel killed processes of it. Left out when not.
    #[serde(default, skip_serializing_if = "is_false")]
    pub out_of_memory: bool,
    /// The Runner's own end kind of the machine: `timeout`, `guest-lost`, `evicted-memory` and so on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    /// The machine the job ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineInfo>,
}

/// What came of a command the console sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandResult {
    /// The command's id, 1 to 64 characters.
    pub id: String,
    /// Whether it worked.
    pub ok: bool,
    /// What to tell the owner, at most 1000 characters kept.
    pub message: String,
    /// When it was done.
    pub at: String,
}

/// What the Mac tells the console every few seconds. `jobs` is filled by [`crate::Client::deliver`] from the outbox.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Report {
    /// What the Mac does now.
    pub status: Status,
    /// What the Mac as a whole is doing.
    pub activity: Activity,
    /// The Mac's host name.
    pub hostname: String,
    /// The Mac's chip and model, such as `Apple M5 (Mac17,3)`.
    pub model: String,
    /// The settings in effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<Settings>,
    /// The guest image in use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<Image>,
    /// Job events, at most 500 in one report.
    pub jobs: Vec<JobEvent>,
    /// What came of commands, at most 50.
    pub results: Vec<CommandResult>,
}

/// A command the console wants the Mac to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    /// Its id, which the result names.
    pub id: String,
    /// `restart`, `update`, `rebuild-image`, `stop-runner` or `settings`; a kind not known here is answered with a
    /// failed [`CommandResult`].
    pub kind: String,
    /// What the kind needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<serde_json::Value>,
}

/// The console's answer to a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    /// Whether the console has paused the Mac.
    pub paused: bool,
    /// Seconds until the next report: 5 while someone watches the console or a command waits, 60 otherwise.
    pub report_every: u64,
    /// Commands to run. They come again with every answer until a report carries their result.
    pub commands: Vec<Command>,
}

#[allow(clippy::trivially_copy_pass_by_ref)] // serde passes a reference
fn is_zero(n: &u64) -> bool {
    *n == 0
}

use serde::{Deserialize, Serialize};

use cf_secret::Secret;

/// What the host sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum HostMessage {
    /// The task of this machine. Sent once, after the supervisor's hello.
    Start(Start),
    /// End the task before it ends by itself.
    Stop(Stop),
}

/// The task and what the guest cannot know by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Start {
    /// [`crate::PROTOCOL`] of the host.
    pub protocol: u32,
    /// The host's wall clock in nanoseconds since 1970 (UTC). The guest sets its clock from it once and never
    /// steps it again.
    pub time_ns: u64,
    /// What to do.
    pub task: Task,
}

/// What a machine is booted for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Task {
    /// Nothing: say that the guest is up and power off. The boot proof and a release's self-test.
    Probe,
    /// Make an empty ext4 file system on the scratch disk with the image's own `mke2fs`, then power off. The result
    /// is the template every job's scratch disk is cloned from.
    FormatScratch,
    /// Run one job.
    Job(Job),
}

/// One job: a workload in a container with limits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Job {
    /// The machine's host name inside the guest (letters, digits and `-`).
    pub name: String,
    /// What the Mac is called in what the job is told ("This job runs on mac-1 with ...").
    pub host: String,
    /// The process to run.
    pub workload: Workload,
    /// The job's limits.
    pub limits: Limits,
    /// Environment variables added to the workload's (later entries win over the supervisor's defaults). Their values
    /// may be credentials (a test run's token): no debug print shows them; on the wire they are plain pairs.
    #[serde(default, with = "secret_env")]
    pub env: Vec<(String, Secret<String>)>,
    /// How long a step gets to fail by itself after the kernel stopped one of its processes for memory, before the
    /// supervisor stops the rest of the step's processes.
    pub oom_grace_ms: u64,
    /// Networks the job must not reach, beyond those the supervisor keeps it from by itself: the ones the Mac is
    /// in (its local network, a VPN), whatever their range, as `(network, prefix length)`.
    #[serde(default)]
    pub blocked: Vec<([u8; 4], u8)>,
    /// Whether the job gets a Docker daemon of its own, in a sandbox beside its container.
    #[serde(default)]
    pub docker: bool,
    /// Whether the machine has a cache disk ([`crate::CACHE_DISK`]): its repository's package caches and, for a
    /// Docker job, its daemon's data.
    #[serde(default)]
    pub cache: bool,
}

/// A job's limits, enforced by the guest kernel on the job's control group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// `memory.max`: the machine's size minus what the guest itself needs. No swap.
    pub memory_bytes: u64,
    /// `pids.max`.
    pub pids: u32,
    /// The size of `/dev/shm`, which counts towards the memory limit.
    pub shm_bytes: u64,
    /// The CPUs of the machine, for what the job is told.
    pub cpus: u32,
}

/// The process a job runs, as the image's user `runner` in the image's home folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Workload {
    /// GitHub's Actions runner with a one-job configuration: `run.sh --jitconfig <configuration>`.
    ActionsRunner {
        /// The encoded one-job configuration GitHub issued.
        #[serde(with = "on_the_wire")]
        jit_config: Secret<String>,
    },
    /// A command in the image, with the same container and limits. The integration suite's stand-in for the
    /// Actions runner.
    Command {
        /// The program (an absolute path, or a name found on the PATH) and its arguments.
        argv: Vec<String>,
    },
}

/// Ends a task early.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    /// Why, which becomes the job's end.
    pub reason: StopReason,
    /// How long the workload gets to end by itself (the Actions runner uploads its log) before everything is
    /// killed.
    pub grace_ms: u64,
}

/// Why the host ends a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopReason {
    /// Nobody needs the job any more (it was cancelled, or its Actions runner never got a job).
    Cancelled,
    /// The job ran longer than a job may.
    Timeout,
    /// The Mac ran low on memory.
    EvictedMemory,
    /// The Mac ran low on disk.
    EvictedDisk,
    /// The Mac slept for too long.
    HostSlept,
}

/// What the supervisor sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum GuestMessage {
    /// The first message: who is speaking.
    Hello {
        /// [`crate::PROTOCOL`] of the supervisor.
        protocol: u32,
        /// The guest kernel's release.
        kernel: String,
    },
    /// The task's workload has been started (for a job: the Actions runner's process exists).
    Ready {
        /// Milliseconds from the kernel's start to this message, by the guest's clock.
        boot_ms: u64,
    },
    /// Every two seconds while a job runs.
    Heartbeat(Usage),
    /// The kernel stopped a process of the job for memory.
    Oom(OomEvent),
    /// The task is over. The last message; the machine powers off after it.
    Exited(Summary),
    /// The task could not be started. The machine powers off after it.
    Failed {
        /// What the supervisor was doing: `protocol`, `disks`, `network`, `firewall`, `cgroup`, `container`.
        stage: String,
        /// What went wrong.
        message: String,
    },
}

impl GuestMessage {
    /// The message as a host keeps it: every text in it cut to its length ([`STAGE_CHARS`], [`MESSAGE_CHARS`],
    /// [`NAME_CHARS`]) with no control character and none that steers the text around it left, and at most
    /// [`LARGEST_PROCESSES`] processes in a report. What a guest says goes into state files, logs and onto
    /// screens, and a guest runs a stranger's code.
    /// [`crate::read_guest_message`] applies it to everything it reads.
    #[must_use]
    pub fn clipped(self) -> Self {
        match self {
            Self::Hello { protocol, kernel } => Self::Hello {
                protocol,
                kernel: clip(&kernel, NAME_CHARS),
            },
            Self::Oom(event) => Self::Oom(event.clipped()),
            Self::Exited(mut summary) => {
                summary.oom = summary.oom.map(OomEvent::clipped);
                Self::Exited(summary)
            }
            Self::Failed { stage, message } => Self::Failed {
                stage: clip(&stage, STAGE_CHARS),
                message: clip(&message, MESSAGE_CHARS),
            },
            Self::Ready { .. } | Self::Heartbeat(_) => self,
        }
    }
}

/// The most characters a host keeps of the stage a guest says it failed at.
pub const STAGE_CHARS: usize = 32;
/// The most characters a host keeps of why a guest says it failed.
pub const MESSAGE_CHARS: usize = 1024;
/// The most characters a host keeps of a name a guest sends: a process, the kernel's release.
pub const NAME_CHARS: usize = 64;
/// The most processes an out-of-memory report names.
pub const LARGEST_PROCESSES: usize = 5;

/// `text` as a host may show it: at most `most` characters, each control character and each one that steers the
/// text around it ([`steers`]) a space.
#[must_use]
pub fn clip(text: &str, most: usize) -> String {
    text.chars()
        .take(most)
        .map(|c| if c.is_control() || steers(c) { ' ' } else { c })
        .collect()
}

/// Whether a character shows nothing itself and changes how the text around it reads: a direction mark or
/// override, a zero-width one, a line or paragraph separator, a tag. A log line with one can read as another.
fn steers(c: char) -> bool {
    matches!(
        c,
        '\u{61c}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// What a job uses at one moment.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Memory charged to the job now.
    pub memory_bytes: u64,
    /// The most memory ever charged to the job.
    pub memory_peak_bytes: u64,
    /// CPU time the job has used, in microseconds.
    pub cpu_usec: u64,
    /// Bytes the job has written to its scratch disk and not deleted.
    pub disk_bytes: u64,
    /// Processes and threads of the job.
    pub pids: u32,
}

/// One process stopped for memory, and who held the memory at that moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OomEvent {
    /// The process the kernel stopped.
    pub process: Process,
    /// The limit the job ran into.
    pub limit_bytes: u64,
    /// The job's processes with the most memory at that moment, largest first, [`LARGEST_PROCESSES`] at most.
    pub largest: Vec<Process>,
}

impl OomEvent {
    fn clipped(mut self) -> Self {
        self.process.name = clip(&self.process.name, NAME_CHARS);
        self.largest.truncate(LARGEST_PROCESSES);
        for process in &mut self.largest {
            process.name = clip(&process.name, NAME_CHARS);
        }
        self
    }
}

/// A process of the job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Process {
    /// Its id inside the guest.
    pub pid: u32,
    /// Its name as the kernel has it (15 characters at most).
    pub name: String,
    /// Its resident memory.
    pub memory_bytes: u64,
}

/// How a task ended and what it used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    /// How it ended.
    pub end: GuestEnd,
    /// The workload's exit code, when it exited by itself.
    pub exit_code: Option<i32>,
    /// The signal that ended the workload, when one did.
    pub signal: Option<i32>,
    /// The last usage; its peak is the job's peak.
    pub usage: Usage,
    /// The most the job ever had on its scratch disk.
    pub disk_peak_bytes: u64,
    /// The first process stopped for memory, when one was.
    pub oom: Option<OomEvent>,
    /// From the workload's start to its end, in milliseconds.
    pub wall_ms: u64,
    /// The job's cache disk: what the guest found on it and how it was let go.
    #[serde(default)]
    pub cache: Option<CacheEnd>,
}

/// What became of a job's cache disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheEnd {
    /// What the disk held when the job started.
    pub found: CacheFound,
    /// Whether the disk was let go cleanly, after a Docker job's daemon stopped by itself: only such a disk is worth
    /// keeping.
    pub clean: bool,
    /// Whether a Docker job's daemon still answered at the job's end; none for a job without Docker. A cache its
    /// daemon could not run on is not used again.
    #[serde(default)]
    pub docker: Option<bool>,
}

/// What a job's cache disk held when the job started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheFound {
    /// Nothing: the repository's first cache, made here.
    Fresh,
    /// A cache of earlier jobs, mounted as it was.
    Reused,
    /// Something that would not mount, replaced by an empty cache.
    Replaced,
}

/// How a task ended, as the guest saw it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reason", rename_all = "kebab-case")]
pub enum GuestEnd {
    /// The workload ended by itself.
    Exited,
    /// The host asked for the end.
    Stopped(StopReason),
}

/// Environment variables whose values are [`Secret`]s, written as the `[name, value]` pairs they always were.
pub mod secret_env {
    use cf_secret::Secret;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Writes the pairs with their values.
    pub fn serialize<S: Serializer>(env: &[(String, Secret<String>)], to: S) -> Result<S::Ok, S::Error> {
        to.collect_seq(env.iter().map(|(name, value)| (name, value.expose())))
    }

    /// Reads the pairs, each value a secret.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        from: D,
    ) -> Result<Vec<(String, Secret<String>)>, D::Error> {
        Vec::<(String, String)>::deserialize(from).map(|env| {
            env.into_iter()
                .map(|(name, value)| (name, Secret::new(value)))
                .collect()
        })
    }
}

/// A secret as a plain string in a message: the one place a [`Secret`] is written out, to the end that needs it.
mod on_the_wire {
    use cf_secret::Secret;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(secret: &Secret<String>, to: S) -> Result<S::Ok, S::Error> {
        to.serialize_str(secret.expose())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(from: D) -> Result<Secret<String>, D::Error> {
        String::deserialize(from).map(Secret::new)
    }
}

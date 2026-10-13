//! What the daemon asks a vm-host for.

use cf_guest_proto::Workload;
use serde::{Deserialize, Serialize};

/// The memory of a machine that is not the job's: the guest's kernel and the supervisor. A job's limit is its
/// machine's size less this.
pub const VM_OVERHEAD: u64 = 256 << 20;

/// One job, as the daemon hands it to a vm-host on its input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRequest {
    /// The job's name on this host: lowercase letters, digits and `-`, at most 32. Names its files and its machine.
    pub job: String,
    /// What the host is called, for what the job is told.
    pub host: String,
    /// The machine's memory, the size the job asked for.
    pub memory_bytes: u64,
    /// The machine's CPUs.
    pub cpus: u32,
    /// The size of the job's disk.
    pub scratch_bytes: u64,
    /// The version of the installed image to run in.
    pub image: String,
    /// What runs: GitHub's Actions runner with its registration, or a command.
    pub workload: Workload,
    /// Variables for the workload on top of the image's own.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// The longest the job may run, in seconds.
    pub deadline_s: u64,
}

/// Why a request is refused before anything starts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    /// The request is not JSON of this shape.
    #[error("the request is malformed: {0}")]
    Malformed(String),
    /// A field has a value no job can have.
    #[error("the request is refused: {0}")]
    Refused(&'static str),
}

fn plain(name: &str, longest: usize) -> bool {
    !name.is_empty()
        && name.len() <= longest
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'.')
        && !name.starts_with(['-', '.'])
}

impl JobRequest {
    /// Reads a request and checks it.
    pub fn parse(json: &str) -> Result<Self, RequestError> {
        // The parser's message may quote the input, which holds the registration: only where it failed is kept.
        let request: Self = serde_json::from_str(json).map_err(|error| {
            RequestError::Malformed(format!("line {}, column {}", error.line(), error.column()))
        })?;
        request.check()?;
        Ok(request)
    }

    /// Checks every field against what a job can have. The names end up in paths, so they are plain.
    pub fn check(&self) -> Result<(), RequestError> {
        let refuse = |why| Err(RequestError::Refused(why));
        if !plain(&self.job, 32) || self.job.contains('.') {
            return refuse("the job's name is not 1 to 32 lowercase letters, digits and dashes");
        }
        if !cf_image::store::is_version(&self.image) {
            return refuse("the image's version is not a plain name");
        }
        if self.host.is_empty() || self.host.len() > 64 || self.host.chars().any(char::is_control) {
            return refuse("the host's name is empty, too long or has control characters");
        }
        if !(512 << 20..=64 << 30).contains(&self.memory_bytes) {
            return refuse("the memory is not between 512 MB and 64 GB");
        }
        if !(1..=32).contains(&self.cpus) {
            return refuse("the CPUs are not between 1 and 32");
        }
        if !(1 << 30..=512 << 30).contains(&self.scratch_bytes) {
            return refuse("the disk is not between 1 GB and 512 GB");
        }
        if !(1..=24 * 3600).contains(&self.deadline_s) {
            return refuse("the deadline is not between a second and a day");
        }
        if let Workload::Command { argv } = &self.workload
            && argv.is_empty()
        {
            return refuse("the command is empty");
        }
        Ok(())
    }

    /// The limits the guest holds the job to.
    pub fn limits(&self) -> cf_guest_proto::Limits {
        cf_guest_proto::Limits {
            memory_bytes: self.memory_bytes - VM_OVERHEAD,
            pids: 4096,
            shm_bytes: (1 << 30).min(self.memory_bytes / 2),
            cpus: self.cpus,
        }
    }
}

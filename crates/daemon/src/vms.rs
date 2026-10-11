//! The vm-hosts, from the daemon's side: one process a job, started with the job's request on its input and asked
//! through its state file and its socket (`cf_vmhost`'s contract).

use cf_guest_proto::StopReason;
use cf_vmhost::control::{self, Request};
use cf_vmhost::{JobRequest, JobState, Layout};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What is there of a job's machine at one moment.
#[derive(Debug, Clone, PartialEq)]
pub enum Seen {
    /// No state: the vm-host has not written any yet, or nothing of the job is left.
    Nothing,
    /// The vm-host this daemon started is gone and left no state: it refused its job before it began.
    Exited,
    /// The vm-host answers, with this state.
    Alive(JobState),
    /// The state file is there and a process has the number in it, but no vm-host answers: it hangs, or the number
    /// is another process's by now.
    Silent(JobState),
    /// The state file is there and its process is not: the vm-host ended (the state then has its `end`) or was
    /// killed (it has none).
    Gone(JobState),
}

/// A vm-host could not be started or reached.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct VmError(pub String);

/// Starts, asks and ends the vm-hosts of this Mac's jobs. A job's name is its handle everywhere.
pub trait Vms: Send + Sync {
    /// Starts a vm-host for the job. After an error none runs.
    fn start(&self, request: &JobRequest) -> Result<(), VmError>;

    /// What is there of the job.
    fn look(&self, job: &str) -> Seen;

    /// Asks the job's vm-host to end it for this reason, with `grace` for the job to end by itself. `false` when
    /// no vm-host took the request.
    fn stop(&self, job: &str, reason: StopReason, grace: Duration) -> bool;

    /// Ends the job's vm-host by force, and its machine with it. Only a process known to be that vm-host is sent
    /// anything.
    fn kill(&self, job: &str);

    /// Every job that has anything in the state folder: a state file, a socket or a machine's folder.
    fn jobs(&self) -> Vec<String>;

    /// Removes what a job whose vm-host is gone left: its state, its socket, its machine's folder with its disk.
    /// The console log stays.
    fn clean(&self, job: &str);
}

/// How long a vm-host has to answer on its socket. It answers one client at a time within a second.
const ANSWER: Duration = Duration::from_millis(1500);

/// The real vm-hosts: `<program> vm --state-dir <folder>`, each a session of its own that outlives the daemon.
#[derive(Debug, Clone)]
pub struct VmHosts {
    program: PathBuf,
    state_dir: PathBuf,
    layout: Layout,
    /// The jobs whose vm-host, started here, has exited.
    exited: Arc<Mutex<BTreeSet<String>>>,
    /// The process numbers of the vm-hosts started here whose exit nobody collected yet: while one is in here
    /// its number is still its own.
    children: Arc<Mutex<BTreeMap<String, u32>>>,
}

impl VmHosts {
    /// The vm-hosts `program` (the Runner's own binary) starts in this state folder.
    pub fn new(program: impl Into<PathBuf>, state_dir: impl AsRef<Path>) -> VmHosts {
        VmHosts {
            program: program.into(),
            state_dir: state_dir.as_ref().to_path_buf(),
            layout: Layout::new(state_dir),
            exited: Arc::default(),
            children: Arc::default(),
        }
    }

    /// Whether the process with this number runs the Runner's binary: all that makes a silent process a vm-host
    /// to end. A number that went to another program is never sent a signal.
    fn is_ours(&self, pid: u32) -> bool {
        process_path(pid).is_some_and(|path| same_file(&path, &self.program))
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Whether a process has this number. One of another user counts: the number is taken.
fn exists(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 1 {
        return false;
    }
    // SAFETY: signal 0 sends nothing; the call only says whether the process is there.
    let answer = unsafe { libc::kill(pid, 0) };
    answer == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The program a process runs.
#[cfg(target_os = "macos")]
fn process_path(pid: u32) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt as _;
    let mut buffer = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: the buffer is this function's own and its true size is passed; the call writes at most that much.
    let length = unsafe {
        libc::proc_pidpath(
            libc::c_int::try_from(pid).ok()?,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).ok()?,
        )
    };
    let length = usize::try_from(length).ok().filter(|n| *n > 0)?;
    Some(PathBuf::from(std::ffi::OsStr::from_bytes(&buffer[..length])))
}

#[cfg(not(target_os = "macos"))]
fn process_path(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/exe")).ok()
}

impl Vms for VmHosts {
    fn start(&self, request: &JobRequest) -> Result<(), VmError> {
        let fail = |what: &str, error: &dyn std::fmt::Display| VmError(format!("{what}: {error}"));
        // The request holds the job's registration: it goes to the vm-host's input and nowhere else.
        let json = serde_json::to_vec(request).map_err(|e| fail("the request", &e))?;
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.layout.logs().join(format!("{}.vm-host.log", request.job)))
            .or_else(|_| {
                std::fs::create_dir_all(self.layout.logs())?;
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.layout.logs().join(format!("{}.vm-host.log", request.job)))
            })
            .map_err(|e| fail("the vm-host's log", &e))?;
        let mut child = Command::new(&self.program)
            .arg("vm")
            .arg("--state-dir")
            .arg(&self.state_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .map_err(|e| fail("start the vm-host", &e))?;
        let written = child
            .stdin
            .take()
            .ok_or_else(|| VmError("the vm-host has no input".into()))
            .and_then(|mut input| {
                input
                    .write_all(&json)
                    .map_err(|e| fail("hand the vm-host its job", &e))
            });
        if let Err(error) = written {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        // The vm-host is a session of its own and ends in its own time: only its exit is collected, on a thread
        // that holds nothing else, so that it is no zombie whose number looks alive.
        let (exited, children, job) = (self.exited.clone(), self.children.clone(), request.job.clone());
        children.lock().expect("children").insert(job.clone(), child.id());
        std::thread::spawn(move || {
            // The number leaves the list after the exit is collected and under the list's lock, so that a signal
            // to a number from the list never reaches another process.
            let _ = child.wait();
            let mut children = children.lock().expect("children");
            children.remove(&job);
            exited.lock().expect("exited").insert(job);
        });
        Ok(())
    }

    fn look(&self, job: &str) -> Seen {
        let Ok(state) = JobState::read(&self.layout, job) else {
            return if self.exited.lock().expect("exited").contains(job) {
                Seen::Exited
            } else {
                Seen::Nothing
            };
        };
        if state.end.is_some() && !exists(state.pid) {
            return Seen::Gone(state);
        }
        match control::ask(&self.layout, job, &Request::State, ANSWER) {
            Ok(now) if now.job == state.job && now.pid == state.pid => Seen::Alive(now),
            // A number that is another program's by now is no vm-host of ours, silent or not.
            _ if state.end.is_none() && exists(state.pid) && self.is_ours(state.pid) => Seen::Silent(state),
            // It ended between the two looks: the file has the last word.
            _ => Seen::Gone(JobState::read(&self.layout, job).unwrap_or(state)),
        }
    }

    fn stop(&self, job: &str, reason: StopReason, grace: Duration) -> bool {
        let grace_ms = u64::try_from(grace.as_millis()).unwrap_or(u64::MAX);
        control::ask(&self.layout, job, &Request::Stop { reason, grace_ms }, ANSWER).is_ok()
    }

    fn kill(&self, job: &str) {
        let Ok(state) = JobState::read(&self.layout, job) else {
            // It wrote nothing yet, so only the Runner that started it knows its number.
            let children = self.children.lock().expect("children");
            if let Some(pid) = children.get(job).and_then(|pid| libc::pid_t::try_from(*pid).ok()) {
                // SAFETY: the number is a child of this process whose exit was not collected (the list's lock is
                // held), so it is that vm-host and nothing else; SIGKILL ends it.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            return;
        };
        let answers = control::answers(&self.layout, &state, ANSWER);
        if !(answers || self.is_ours(state.pid)) {
            return;
        }
        if let Ok(pid) = libc::pid_t::try_from(state.pid)
            && pid > 1
        {
            // SAFETY: the number is a process that answered as this job's vm-host, or that runs the Runner's
            // binary; SIGKILL to it ends the vm-host and, with it, the machine the hypervisor ran for it.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }

    fn jobs(&self) -> Vec<String> {
        let mut jobs: Vec<String> = std::fs::read_dir(self.layout.run())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                let job = name
                    .strip_suffix(".json")
                    .or_else(|| name.strip_suffix(".sock"))
                    .unwrap_or(&name);
                (!job.is_empty() && !job.contains('.')).then(|| job.to_string())
            })
            .collect();
        jobs.sort();
        jobs.dedup();
        jobs
    }

    fn clean(&self, job: &str) {
        let _ = std::fs::remove_file(self.layout.socket(job));
        let _ = std::fs::remove_dir_all(self.layout.job_dir(job));
        let _ = std::fs::remove_file(self.layout.state(job));
        self.exited.lock().expect("exited").remove(job);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Instant;

    use cf_guest_proto::Workload;

    use super::*;

    fn request(job: &str) -> JobRequest {
        JobRequest {
            job: job.into(),
            host: "mac-a".into(),
            memory_bytes: 2 << 30,
            cpus: 2,
            scratch_bytes: 1 << 30,
            image: "2026.10.1".into(),
            workload: Workload::Command {
                argv: vec!["true".into()],
            },
            env: Vec::new(),
            deadline_s: 60,
        }
    }

    /// A program in place of the Runner's binary, as a vm-host that hangs before it wrote anything.
    fn hanging(folder: &Path) -> PathBuf {
        let program = folder.join("vm-host");
        std::fs::write(&program, "#!/bin/sh\nexec sleep 60\n").expect("a program");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).expect("a mode");
        program
    }

    fn within(limit: Duration, mut holds: impl FnMut() -> bool) -> bool {
        let began = Instant::now();
        while began.elapsed() < limit {
            if holds() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        holds()
    }

    #[test]
    fn a_vm_host_that_hangs_before_it_wrote_anything_is_still_killed() {
        let folder = tempfile::tempdir().expect("folder");
        let vms = VmHosts::new(hanging(folder.path()), folder.path().join("state"));
        vms.start(&request("j1")).expect("started");
        assert!(
            matches!(vms.look("j1"), Seen::Nothing),
            "it wrote nothing and has not exited"
        );
        // A job nobody started here names no process: nothing is sent anywhere.
        vms.kill("j2");
        assert!(matches!(vms.look("j1"), Seen::Nothing));
        vms.kill("j1");
        assert!(
            within(Duration::from_secs(5), || matches!(vms.look("j1"), Seen::Exited)),
            "the vm-host outlived its kill"
        );
        // Its number is nobody's now: a second kill sends nothing.
        assert!(vms.children.lock().expect("children").is_empty());
        vms.kill("j1");
    }
}

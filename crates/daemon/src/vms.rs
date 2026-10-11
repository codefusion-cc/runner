//! The vm-hosts, from the daemon's side: one process a job, started with the job's request on its input and asked
//! through its state file and its socket (`cf_vmhost`'s contract).

use cf_guest_proto::StopReason;
use cf_vmhost::control::{self, Request};
use cf_vmhost::{JobRequest, JobState, Layout};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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

    /// Every job that has anything in the state folder: a state file, a socket, a machine's folder, or the note of
    /// a vm-host that was started for it.
    fn jobs(&self) -> Vec<String>;

    /// Removes what a job whose vm-host is gone left: its state, its socket, its machine's folder with its disk,
    /// the note of its process. The console log stays.
    fn clean(&self, job: &str);
}

/// How long a vm-host has to answer on its socket. It answers one client at a time within a second.
const ANSWER: Duration = Duration::from_millis(1500);

/// How much later than the time written down for it a vm-host's process may have started: the two times are read
/// from the same clock a moment apart and rounded to seconds.
const START_SLACK: u64 = 5;

/// How long a kill waits for a vm-host of a Runner before to be gone, so that what it left is removed after it.
const GONE_WAIT: Duration = Duration::from_secs(2);

/// What the Runner writes down of a vm-host it started, in `run/<job>.pid`: until the vm-host writes its own
/// state, this is all a Runner that starts next knows of the process.
#[derive(Debug, Serialize, Deserialize)]
struct Spawned {
    /// The vm-host's process.
    pid: u32,
    /// Seconds since 1970 when it was started.
    at: u64,
}

fn now_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

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

    /// Whether the process with this number is the vm-host that was started by `born_by` (seconds since 1970): it
    /// runs the Runner's binary and is no younger than that. All that makes a silent process a vm-host to end. A
    /// number that went to another program is never sent a signal, nor one that went to a later process of the
    /// Runner's own binary (another job's vm-host, a command, this Runner itself).
    fn is_ours(&self, pid: u32, born_by: u64) -> bool {
        pid != std::process::id()
            && process_path(pid).is_some_and(|path| same_file(&path, &self.program))
            && process_start(pid).is_some_and(|started| started <= born_by.saturating_add(START_SLACK))
    }

    fn spawned(&self, job: &str) -> PathBuf {
        self.layout.run().join(format!("{job}.pid"))
    }

    /// Writes down the process a job's vm-host was started as.
    fn note(&self, job: &str, pid: u32) -> std::io::Result<()> {
        use std::os::unix::fs::DirBuilderExt as _;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(self.layout.run())?;
        let spawned = serde_json::to_vec(&Spawned { pid, at: now_s() }).map_err(std::io::Error::other)?;
        cf_secret::replace_private(&self.spawned(job), &spawned)
    }

    /// The vm-host a Runner before started for the job and wrote down, when its process is still that one.
    fn noted(&self, job: &str) -> Option<u32> {
        let spawned: Spawned = serde_json::from_slice(&std::fs::read(self.spawned(job)).ok()?).ok()?;
        self.is_ours(spawned.pid, spawned.at).then_some(spawned.pid)
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

/// When a process started, in seconds since 1970.
#[cfg(target_os = "macos")]
fn process_start(pid: u32) -> Option<u64> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: `info` is `size` writable bytes, which is what is passed; the call writes at most that many and says
    // how many it wrote.
    let written = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if written != size {
        return None;
    }
    // SAFETY: the call wrote the whole struct, which is integers and bytes and was zeroed before.
    Some(unsafe { info.assume_init() }.pbi_start_tvsec)
}

#[cfg(not(target_os = "macos"))]
fn process_start(pid: u32) -> Option<u64> {
    let started = std::fs::metadata(format!("/proc/{pid}")).ok()?.modified().ok()?;
    Some(started.duration_since(UNIX_EPOCH).ok()?.as_secs())
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
        // Written down before the vm-host is handed its job: a Runner that starts next knows the process even
        // when it never writes a state of its own.
        let written = self
            .note(&request.job, child.id())
            .map_err(|e| fail("write down the vm-host's process", &e))
            .and_then(|()| {
                child
                    .stdin
                    .take()
                    .ok_or_else(|| VmError("the vm-host has no input".into()))
            })
            .and_then(|mut input| {
                input
                    .write_all(&json)
                    .map_err(|e| fail("hand the vm-host its job", &e))
            });
        if let Err(error) = written {
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_file(self.spawned(&request.job));
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
            _ if state.end.is_none() && exists(state.pid) && self.is_ours(state.pid, state.started_at) => {
                Seen::Silent(state)
            }
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
                return;
            }
            drop(children);
            // Or the Runner before this one did, and wrote the number down.
            let Some(noted) = self.noted(job) else {
                return;
            };
            if let Ok(pid) = libc::pid_t::try_from(noted) {
                // SAFETY: the number is a process that runs the Runner's binary and is as old as the vm-host
                // written down for this job; SIGKILL ends it before it boots a machine.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                // What it left is removed next: not before it is gone, or it writes into a folder that is nobody's.
                let until = Instant::now() + GONE_WAIT;
                while process_path(noted).is_some() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            return;
        };
        let answers = control::answers(&self.layout, &state, ANSWER);
        if !(answers || self.is_ours(state.pid, state.started_at)) {
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
                let job = [".json", ".sock", ".pid"]
                    .iter()
                    .find_map(|suffix| name.strip_suffix(suffix))
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
        let _ = std::fs::remove_file(self.spawned(job));
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

    /// A program in place of the Runner's binary that a process can be seen to run. Not a copy of it under another
    /// name: macOS ends a copy of one of its own programs.
    const SLEEP: &str = "/bin/sleep";

    /// A process of the program, as a vm-host that hangs. It never outlives its test.
    struct Asleep(std::process::Child);

    impl Asleep {
        fn start(program: &Path) -> Asleep {
            let child = Command::new(program)
                .arg("60")
                .stdout(Stdio::null())
                .spawn()
                .expect("a process");
            let asleep = Asleep(child);
            assert!(
                within(Duration::from_secs(5), || process_path(asleep.id())
                    .is_some_and(|path| same_file(&path, program))),
                "the process runs the program"
            );
            asleep
        }

        fn id(&self) -> u32 {
            self.0.id()
        }

        fn runs(&mut self) -> bool {
            self.0.try_wait().expect("a child").is_none()
        }

        fn killed(&mut self) -> bool {
            use std::os::unix::process::ExitStatusExt as _;
            within(Duration::from_secs(5), || {
                self.0
                    .try_wait()
                    .expect("a child")
                    .is_some_and(|status| status.signal() == Some(libc::SIGKILL))
            })
        }
    }

    impl Drop for Asleep {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn state(job: &str, pid: u32, started_at: u64) -> JobState {
        JobState {
            job: job.into(),
            pid,
            started_at,
            stage: cf_vmhost::Stage::Booting,
            vm_bytes: 2 << 30,
            limit_bytes: 1 << 30,
            cpus: 2,
            image: "2026.10.1".into(),
            boots: 0,
            boot_ms: None,
            usage: None,
            oom: None,
            end: None,
        }
    }

    fn write_state(vms: &VmHosts, state: &JobState) {
        std::fs::create_dir_all(vms.layout.run()).expect("folder");
        std::fs::write(
            vms.layout.state(&state.job),
            serde_json::to_vec(state).expect("json"),
        )
        .expect("a state");
    }

    #[test]
    fn a_start_writes_the_process_down_and_what_is_removed_after_the_job_takes_the_note_with_it() {
        let folder = tempfile::tempdir().expect("folder");
        let vms = VmHosts::new(hanging(folder.path()), folder.path().join("state"));
        vms.start(&request("j1")).expect("started");
        let noted: Spawned =
            serde_json::from_slice(&std::fs::read(vms.spawned("j1")).expect("a note")).expect("json");
        assert_eq!(
            Some(&noted.pid),
            vms.children.lock().expect("children").get("j1"),
            "the note names the vm-host"
        );
        let mode = std::fs::metadata(vms.spawned("j1"))
            .expect("the note")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            vms.jobs(),
            ["j1"],
            "a job with a note and nothing else is still a job to look at"
        );
        vms.kill("j1");
        assert!(within(Duration::from_secs(5), || matches!(
            vms.look("j1"),
            Seen::Exited
        )));
        vms.clean("j1");
        assert!(!vms.spawned("j1").exists());
        assert_eq!(vms.jobs(), Vec::<String>::new());
    }

    #[test]
    fn a_vm_host_of_a_runner_before_that_wrote_nothing_is_killed_by_the_note_of_its_process() {
        let folder = tempfile::tempdir().expect("folder");
        let program = Path::new(SLEEP);
        let before = VmHosts::new(program, folder.path().join("state"));
        let mut vm_host = Asleep::start(program);
        before.note("j1", vm_host.id()).expect("a note");
        // The Runner that starts next: it has no child, only the folder.
        let next = VmHosts::new(program, folder.path().join("state"));
        assert_eq!(next.jobs(), ["j1"]);
        assert!(matches!(next.look("j1"), Seen::Nothing), "it wrote no state");
        next.kill("j2");
        assert!(vm_host.runs(), "a job without a note names nobody");
        next.kill("j1");
        assert!(vm_host.killed(), "the vm-host outlived its kill");
        next.clean("j1");
        assert_eq!(next.jobs(), Vec::<String>::new());
    }

    #[test]
    fn a_number_that_went_to_a_later_process_of_the_runners_own_program_is_sent_nothing() {
        let folder = tempfile::tempdir().expect("folder");
        let program = Path::new(SLEEP);
        let vms = VmHosts::new(program, folder.path().join("state"));
        // The vm-hosts of before are long gone, and their numbers are a later process's: of the same program, as
        // another job's vm-host is, or a command, or the Runner itself.
        let mut later = Asleep::start(program);
        let long_ago = now_s() - 3600;
        write_state(&vms, &state("j1", later.id(), long_ago));
        std::fs::write(
            vms.spawned("j2"),
            serde_json::to_vec(&Spawned {
                pid: later.id(),
                at: long_ago,
            })
            .expect("json"),
        )
        .expect("a note");
        assert!(
            matches!(vms.look("j1"), Seen::Gone(_)),
            "the process is not the one the state names: {:?}",
            vms.look("j1")
        );
        vms.kill("j1");
        vms.kill("j2");
        std::thread::sleep(Duration::from_millis(300));
        assert!(later.runs(), "the later process was killed");

        // The same process under a state of its own time is the vm-host that hangs: silent, and killed.
        write_state(&vms, &state("j4", later.id(), now_s()));
        assert!(matches!(vms.look("j4"), Seen::Silent(_)), "{:?}", vms.look("j4"));
        vms.kill("j4");
        assert!(later.killed(), "the vm-host that hangs outlived its kill");
    }

    #[test]
    fn the_runners_own_process_is_never_a_vm_host_to_end_whatever_a_file_says() {
        let folder = tempfile::tempdir().expect("folder");
        let itself = std::env::current_exe().expect("this program");
        let vms = VmHosts::new(&itself, folder.path().join("state"));
        let me = std::process::id();
        assert!(process_path(me).is_some_and(|path| same_file(&path, &itself)));
        write_state(&vms, &state("j1", me, now_s()));
        assert!(matches!(vms.look("j1"), Seen::Gone(_)), "{:?}", vms.look("j1"));
        vms.kill("j1");
        std::fs::remove_file(vms.layout.state("j1")).expect("the state");
        std::fs::write(
            vms.spawned("j1"),
            serde_json::to_vec(&Spawned { pid: me, at: now_s() }).expect("json"),
        )
        .expect("a note");
        assert_eq!(vms.noted("j1"), None);
        vms.kill("j1");
    }
}

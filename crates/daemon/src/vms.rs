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
use std::time::{Duration, Instant, SystemTime};

use crate::platform::{Platform as _, Process, This};

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
    /// When its process started by the system's clock that no step of the wall clock moves ([`Process::mark`]),
    /// where the system keeps one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    mark: Option<u64>,
}

fn now_s() -> u64 {
    u64::try_from(crate::secs(SystemTime::now())).unwrap_or(0)
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
    /// `systemd-run`, when each vm-host starts in a systemd scope of its own ([`VmHosts::in_scopes`]).
    systemd_run: Option<PathBuf>,
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
            systemd_run: None,
        }
    }

    /// The same vm-hosts, each started by `systemd_run` in a transient scope of the user's systemd,
    /// `codefusion-runner-<job>.scope`, whose cgroup is delegated to it: there it makes its machines' cgroups. The
    /// scope is a unit of its own, so a vm-host outlives the Runner's service whatever systemd does to it, and the
    /// service's own cgroup never has controllers enabled for its children, which would keep systemd from starting
    /// the service in it again. The process is the vm-host itself (`systemd-run --scope` becomes it), so its number
    /// is the one written down.
    pub fn in_scopes(self, systemd_run: impl Into<PathBuf>) -> VmHosts {
        VmHosts {
            systemd_run: Some(systemd_run.into()),
            ..self
        }
    }

    /// The command that starts the job's vm-host: the Runner's binary, or `systemd_run` with it in a scope named
    /// after the job whose cgroup is delegated to it. The job's name is one [`JobRequest::check`] took.
    fn command(&self, job: &str) -> Result<Command, VmError> {
        let mut command = match &self.systemd_run {
            None => Command::new(&self.program),
            Some(systemd_run) => {
                let mut command = Command::new(systemd_run);
                command
                    .args([
                        "--user",
                        "--scope",
                        "--quiet",
                        "--collect",
                        "--property=Delegate=yes",
                    ])
                    .arg(format!("--unit={}", scope_unit(job)))
                    .arg("--")
                    .arg(&self.program);
                // The bus of the user's systemd, which a Runner started by hand under `sudo -iu` may not be told of.
                #[cfg(target_os = "linux")]
                command.envs(
                    crate::platform::this_user_bus_env()
                        .map_err(|why| VmError(format!("no scope for the vm-host: {why}")))?,
                );
                command
            }
        };
        command.arg("vm").arg("--state-dir").arg(&self.state_dir);
        Ok(command)
    }

    /// Whether the process with this number is the vm-host that was started by `born_by` (seconds since 1970): it
    /// runs the Runner's binary and is no younger than that. All that makes a silent process a vm-host to end. A
    /// number that went to another program is never sent a signal, nor one that went to a later process of the
    /// Runner's own binary (another job's vm-host, a command, this Runner itself).
    ///
    /// `mark` is the start of the vm-host's process as written down when it was started ([`Process::mark`]): where
    /// it is known, the process must have started at that very moment, whatever the wall clock did since.
    fn is_ours(&self, pid: u32, born_by: u64, mark: Option<u64>) -> bool {
        pid != std::process::id()
            && This::process(pid).is_some_and(|process| {
                same_file(&process.program, &self.program) && started_by(&process, born_by, mark)
            })
    }

    fn spawned(&self, job: &str) -> PathBuf {
        self.layout.run().join(format!("{job}.pid"))
    }

    /// Writes down the process a job's vm-host was started as.
    fn note(&self, job: &str, pid: u32) -> std::io::Result<()> {
        cf_secret::private_dir(&self.layout.run())?;
        let spawned = Spawned {
            pid,
            at: now_s(),
            mark: This::process(pid).and_then(|process| process.mark),
        };
        let spawned = serde_json::to_vec(&spawned).map_err(std::io::Error::other)?;
        cf_secret::replace_private(&self.spawned(job), &spawned)
    }

    /// The vm-host a Runner before started for the job and wrote down, when its process is still that one.
    fn noted(&self, job: &str) -> Option<u32> {
        let spawned = self.note_of(job)?;
        self.is_ours(spawned.pid, spawned.at, spawned.mark)
            .then_some(spawned.pid)
    }

    /// What was written down of the job's vm-host when it was started.
    fn note_of(&self, job: &str) -> Option<Spawned> {
        serde_json::from_slice(&std::fs::read(self.spawned(job)).ok()?).ok()
    }

    /// Whether the process `pid` that the job's state names is its vm-host: by the start written down for that
    /// process when it was started, else by the time the state gives.
    fn state_is_ours(&self, job: &str, state: &JobState) -> bool {
        let mark = self
            .note_of(job)
            .filter(|spawned| spawned.pid == state.pid)
            .and_then(|spawned| spawned.mark);
        self.is_ours(state.pid, state.started_at, mark)
    }
}

/// Whether `process` started by the moment `born_by` (seconds since 1970), or, where `mark` is known, at that very
/// start. A wall clock that was stepped since moves what Linux says of an old process's start in seconds (it works it
/// out from the clock at every read) and nothing of its mark: a board without a clock of its own that meets NTP after
/// its vm-hosts started still knows them.
fn started_by(process: &Process, born_by: u64, mark: Option<u64>) -> bool {
    match (mark, process.mark) {
        (Some(noted), Some(now)) => noted == now,
        _ => process.started_at <= born_by.saturating_add(START_SLACK),
    }
}

/// The name of the systemd scope a job's vm-host runs in. A job's name ([`JobRequest::check`]) is one a unit can
/// carry as it is.
fn scope_unit(job: &str) -> String {
    format!("codefusion-runner-{job}.scope")
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

/// Waits until the child `pid` of this process has ended, and leaves its exit to be collected: until someone does,
/// the number goes to no other process. False when the system would not wait this way.
fn ended_uncollected(pid: u32) -> bool {
    loop {
        // SAFETY: `waitid` writes only into `info`, which is a `siginfo_t` of this frame, zeroed as C would have it.
        let waited = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            libc::waitid(
                libc::P_PID,
                libc::id_t::from(pid),
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if waited == 0 {
            return true;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return false;
        }
    }
}

/// The program a process runs.
fn process_path(pid: u32) -> Option<PathBuf> {
    This::program(pid)
}

impl Vms for VmHosts {
    fn start(&self, request: &JobRequest) -> Result<(), VmError> {
        let fail = |what: &str, error: &dyn std::fmt::Display| VmError(format!("{what}: {error}"));
        // The job's name ends up in paths and in the name of a unit.
        request.check().map_err(|e| fail("the request", &e))?;
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
        let mut child = self
            .command(&request.job)?
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
            // The exit is collected under the list's lock, and the number leaves the list with it: a number in the
            // list is a process nobody collected yet, so a signal to it never reaches another process.
            let ended = ended_uncollected(child.id());
            if !ended {
                let _ = child.wait();
            }
            let mut children = children.lock().expect("children");
            if ended {
                let _ = child.wait();
            }
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
            _ if state.end.is_none() && exists(state.pid) && self.state_is_ours(job, &state) => {
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
        if !(answers || self.state_is_ours(job, &state)) {
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
            docker: false,
            docker_cache: None,
        }
    }

    /// A Runner that starts again looks at a vm-host for the first time long after the vm-host started: its start is
    /// when it started, not when it was first looked at (Linux makes `/proc/<pid>` only then, with that time).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_vm_host_first_looked_at_long_after_it_started_is_still_known_as_ours() {
        let folder = tempfile::tempdir().expect("folder");
        let program = Path::new(SLEEP);
        let vms = VmHosts::new(program, folder.path().join("state"));
        let born_by = now_s();
        let mut child = Command::new(program)
            .arg("60")
            .stdout(Stdio::null())
            .spawn()
            .expect("a process");
        // Nothing here looks at the process until well past the slack a start may have.
        std::thread::sleep(Duration::from_secs(START_SLACK + 2));
        let ours = vms.is_ours(child.id(), born_by, None);
        let _ = child.kill();
        let _ = child.wait();
        assert!(ours, "a vm-host that has run for a while is no stranger");
    }

    #[test]
    fn a_process_known_by_its_start_mark_is_that_process_whatever_the_wall_clock_did() {
        let born_by = 1_000_000;
        let process = |started_at: u64, mark: Option<u64>| Process {
            program: PathBuf::from("/bin/codefusion-runner"),
            started_at,
            mark,
        };
        // No mark: started no later than the time written down, with the slack of two reads of one clock.
        assert!(started_by(&process(born_by + START_SLACK, None), born_by, None));
        assert!(!started_by(
            &process(born_by + START_SLACK + 1, None),
            born_by,
            None
        ));
        assert!(started_by(&process(born_by - 3600, Some(7)), born_by, None));
        // The clock was stepped an hour forward since: the seconds moved, the mark did not.
        assert!(started_by(
            &process(born_by + 3600, Some(4200)),
            born_by,
            Some(4200)
        ));
        // The number went to another process, which by the seconds would pass.
        assert!(!started_by(
            &process(born_by - 10, Some(4300)),
            born_by,
            Some(4200)
        ));
        // A system that keeps no mark is judged by the seconds.
        assert!(started_by(&process(born_by, None), born_by, Some(4200)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_vm_host_is_written_down_with_its_start_mark_and_known_by_it() {
        let folder = tempfile::tempdir().expect("folder");
        let program = Path::new(SLEEP);
        let vms = VmHosts::new(program, folder.path().join("state"));
        let mut vm_host = Asleep::start(program);
        vms.note("j1", vm_host.id()).expect("a note");
        let noted = vms.note_of("j1").expect("the note");
        let mark = This::process(vm_host.id()).and_then(|process| process.mark);
        assert!(mark.is_some(), "Linux keeps a mark");
        assert_eq!((noted.pid, noted.mark), (vm_host.id(), mark));
        assert_eq!(vms.noted("j1"), Some(vm_host.id()));
        // A note that names another start of that number names no process to end.
        std::fs::write(
            vms.spawned("j2"),
            serde_json::to_vec(&Spawned {
                pid: vm_host.id(),
                at: now_s(),
                mark: mark.map(|mark| mark + 1),
            })
            .expect("json"),
        )
        .expect("a note");
        assert_eq!(vms.noted("j2"), None);
        vms.kill("j2");
        assert!(vm_host.runs(), "a process that only has the number was killed");
    }

    #[test]
    fn a_job_whose_name_no_path_or_scope_can_carry_is_not_started() {
        let folder = tempfile::tempdir().expect("folder");
        let vms = VmHosts::new("/bin/true", folder.path()).in_scopes("/bin/true");
        for job in [
            "",
            "a.b",
            "a/b",
            "a b",
            "a\\x2d",
            "a_b",
            "J1",
            "ä",
            "a\n",
            &"j".repeat(33),
        ] {
            let refused = vms.start(&request(job)).expect_err(job);
            assert!(
                refused.0.starts_with("the request: the request is refused"),
                "{job:?}: {}",
                refused.0
            );
        }
        assert!(
            !folder.path().join("logs").exists(),
            "a refused job leaves no log"
        );
    }

    #[test]
    fn a_vm_host_in_a_scope_is_the_runners_program_under_systemd_run_with_its_cgroup_delegated() {
        let vms = VmHosts::new("/state/bin/codefusion-runner", "/state").in_scopes("/usr/bin/systemd-run");
        let command = match vms.command("j1a2b3c4") {
            Ok(command) => command,
            // Where no systemd user manager runs for this user, there is no bus to start a scope on.
            Err(error) if cfg!(target_os = "linux") => {
                assert!(error.0.starts_with("no scope for the vm-host: "), "{}", error.0);
                return;
            }
            Err(error) => panic!("{}", error.0),
        };
        assert_eq!(command.get_program(), "/usr/bin/systemd-run");
        let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--property=Delegate=yes",
                "--unit=codefusion-runner-j1a2b3c4.scope",
                "--",
                "/state/bin/codefusion-runner",
                "vm",
                "--state-dir",
                "/state",
            ]
        );
        let direct = VmHosts::new("/state/bin/codefusion-runner", "/state")
            .command("j1a2b3c4")
            .expect("a command");
        assert_eq!(direct.get_program(), "/state/bin/codefusion-runner");
        assert_eq!(
            direct.get_args().collect::<Vec<_>>(),
            ["vm", "--state-dir", "/state"]
        );
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

    /// systemd refuses the scope (a scope of that name stands, the user's systemd will not take another unit): the
    /// vm-host never runs. The start fails, or the job ends as one whose vm-host exited before it began, and
    /// systemd's words are in the vm-host's log, not lost.
    #[test]
    fn a_scope_systemd_refuses_ends_the_job_with_systemds_words_in_its_log() {
        let folder = tempfile::tempdir().expect("folder");
        let refuses = folder.path().join("systemd-run");
        std::fs::write(
            &refuses,
            "#!/bin/sh\necho 'Failed to start transient scope unit: Unit codefusion-runner-j1.scope already exists.' \
             >&2\nexit 1\n",
        )
        .expect("a program");
        std::fs::set_permissions(&refuses, std::fs::Permissions::from_mode(0o755)).expect("a mode");
        let vms = VmHosts::new(hanging(folder.path()), folder.path().join("state")).in_scopes(&refuses);
        match vms.start(&request("j1")) {
            // systemd-run ended after the job was handed over: the next look sees a vm-host gone without a word.
            Ok(()) => assert!(
                within(Duration::from_secs(5), || matches!(vms.look("j1"), Seen::Exited)),
                "a refused scope looks like a vm-host that still starts: {:?}",
                vms.look("j1")
            ),
            // It ended before: the start itself fails.
            Err(error) if error.0.starts_with("hand the vm-host its job: ") => {}
            // Where no systemd user manager runs for this user, the start is refused before systemd is asked.
            Err(error) if cfg!(target_os = "linux") => {
                assert!(error.0.starts_with("no scope for the vm-host: "), "{}", error.0);
                return;
            }
            Err(error) => panic!("{}", error.0),
        }
        let log =
            std::fs::read_to_string(vms.layout.logs().join("j1.vm-host.log")).expect("the vm-host's log");
        assert!(
            log.contains("Unit codefusion-runner-j1.scope already exists."),
            "{log}"
        );
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
    #[test]
    fn a_vm_hosts_exit_is_waited_for_without_giving_its_number_to_another_process() {
        let mut child = Command::new(SLEEP).arg("0.2").spawn().expect("a process");
        let pid = child.id();
        assert!(ended_uncollected(pid));
        assert!(exists(pid), "ended, and still this process's to collect");
        let status = child.try_wait().expect("a child").expect("it has ended");
        assert!(status.success());
        assert!(!exists(pid));
        assert!(
            !ended_uncollected(pid),
            "a number that is no child of this process any more"
        );
    }

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
            resident_bytes: None,
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
                mark: None,
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
            serde_json::to_vec(&Spawned {
                pid: me,
                at: now_s(),
                mark: None,
            })
            .expect("json"),
        )
        .expect("a note");
        assert_eq!(vms.noted("j1"), None);
        vms.kill("j1");
    }
}

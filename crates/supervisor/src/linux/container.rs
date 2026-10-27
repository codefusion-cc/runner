//! The job's container. The supervisor starts its own binary again as the launcher; the launcher makes the
//! namespaces and their first process; that process builds the container's view of the machine, gives up real
//! root, and starts the workload as the image's user.
//!
//! What holds a job in: its processes sit in a control group whose limits are set one level above what the
//! container can see; they are root only in a user namespace made last, so every mount made before is locked,
//! `/proc/sys` and `/sys` are read-only, and the network (shared with the supervisor, so no bridge is needed) and
//! its firewall belong to a namespace their root has no power over.

use super::sys::{self, check, context, fork, unshare, wait};
use cf_supervisor::outcome::Outcome;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// The descriptor the launcher reports on.
pub const STATUS_FD: libc::c_int = 100;

/// What the launcher is told, on its input. It holds the workload's arguments, so it is never printed.
#[derive(Serialize, Deserialize)]
pub struct Spec {
    /// The directory that becomes the container's root.
    pub root: String,
    /// The directory the container sees, read-only, at `/run/codefusion`.
    pub shared: String,
    /// The control group the workload runs in.
    pub cgroup: String,
    /// The workload's program and arguments.
    pub argv: Vec<String>,
    /// The workload's whole environment.
    pub env: Vec<(String, String)>,
    /// The workload's user, group and further groups.
    pub uid: u32,
    /// See `uid`.
    pub gid: u32,
    /// See `uid`.
    pub groups: Vec<u32>,
    /// The directory the workload starts in.
    pub home: String,
    /// The size of `/dev/shm`.
    pub shm_bytes: u64,
    /// The job's cache disk as the supervisor mounted it, which the container sees at the same path; none without
    /// one.
    #[serde(default)]
    pub cache: Option<String>,
}

fn report(outcome: &Outcome) {
    // SAFETY: the supervisor hands the launcher this descriptor; it is wrapped once per process, just before exit.
    let mut status = unsafe { File::from_raw_fd(STATUS_FD) };
    let _ = status.write_all(serde_json::to_string(outcome).unwrap_or_default().as_bytes());
}

/// The launcher's entry.
pub fn launch() -> ! {
    let Err(error) = launcher();
    report(&Outcome::Error(error));
    std::process::exit(70)
}

/// What a launcher is told on its input, read whole; its input is then `/dev/null`. The text may hold what the
/// workload starts with, so neither it nor the parser's message is kept. `what` names it in an error.
pub(super) fn read_spec<T: DeserializeOwned>(what: &str) -> Result<T, String> {
    let mut text = String::new();
    context(
        &format!("read the {what}'s description"),
        std::io::stdin().read_to_string(&mut text),
    )?;
    let spec = serde_json::from_str(&text).map_err(|_| format!("the {what}'s description is malformed"))?;
    drop(text);
    if let Ok(null) = File::open("/dev/null") {
        // SAFETY: dup2 on descriptors we own.
        unsafe { libc::dup2(null.as_raw_fd(), 0) };
    }
    Ok(spec)
}

/// The launcher's side of a user namespace's ids: once the first process asks on `ask`, its maps are written and
/// `done` says so. Closed without the byte, `done` tells the first process that it got no ids, so it does not wait.
pub(super) fn give_ids(first: libc::pid_t, ask: OwnedFd, done: OwnedFd, maps: (&str, &str), what: &str) {
    let mut byte = [0u8; 1];
    let mut done = File::from(done);
    if File::from(ask).read(&mut byte).unwrap_or(0) == 1 {
        match fs::write(format!("/proc/{first}/uid_map"), maps.0)
            .and_then(|()| fs::write(format!("/proc/{first}/gid_map"), maps.1))
        {
            Ok(()) => drop(done.write_all(&[1])),
            Err(error) => eprintln!("cf-supervisor: the {what}'s ids could not be set: {error}"),
        }
    }
}

/// The first process's side, in the user namespace it just made: asks for its ids and waits for them.
pub(super) fn await_ids(ask: OwnedFd, done: OwnedFd, what: &str) -> Result<(), String> {
    context("ask for ids", File::from(ask).write_all(&[1]))?;
    let mut byte = [0u8; 1];
    if File::from(done).read(&mut byte).unwrap_or(0) != 1 {
        return Err(format!("the launcher gave the {what} no ids"));
    }
    Ok(())
}

fn launcher() -> Result<Infallible, String> {
    let spec: Spec = read_spec("container")?;
    // The launcher and the container's first process keep to themselves: the job's root, which shares their
    // user ids, can neither trace them nor open what they hold open (the pipe the job's end is reported on).
    // The workload gets the usual back when it is started as its own program.
    // SAFETY: prctl with this option takes numbers only.
    context(
        "keep to itself",
        check(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }),
    )?;
    let files = libc::rlimit {
        rlim_cur: 65536,
        rlim_max: 1 << 20,
    };
    // SAFETY: setrlimit reads the structure we own.
    unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &files) };
    sys::set(format!("{}/cgroup.procs", spec.cgroup), "0")?;
    // No mount namespace here: the launcher keeps the supervisor's view, where it finds the first process to
    // give it its ids. The first process makes its own.
    unshare(libc::CLONE_NEWPID | libc::CLONE_NEWIPC | libc::CLONE_NEWUTS | libc::CLONE_NEWCGROUP)?;

    let (ask_read, ask_write) = context("pipe", sys::pipe())?;
    let (done_read, done_write) = context("pipe", sys::pipe())?;
    let first = fork()?;
    if first == 0 {
        drop((ask_read, done_write));
        let outcome = match contain(&spec, ask_write, done_read) {
            Ok(status) => Outcome::Status(status),
            Err(error) => Outcome::Error(error),
        };
        report(&outcome);
        std::process::exit(0);
    }
    drop((ask_write, done_read));
    // The kernel's choice under memory pressure is a process of the job's, never the launcher. This is set only
    // now, after the first process was made: a value real root sets is also the lowest its children may set, and
    // a job that could give itself -1000 would hang at its limit with nothing for the kernel to stop.
    let _ = fs::write("/proc/self/oom_score_adj", "-1000");
    // The first process asks once it has its user namespace; only real root can give that namespace every id.
    let whole = "0 0 4294967295\n";
    give_ids(first, ask_read, done_write, (whole, whole), "container");
    let (_, status) = wait(first);
    std::process::exit(sys::exit_code(status))
}

const LOCKED: libc::c_ulong = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;

/// The container's first process: builds the view, drops real root, runs the workload and reaps what it leaves.
fn contain(spec: &Spec, ask: OwnedFd, done: OwnedFd) -> Result<libc::c_int, String> {
    // SAFETY: fcntl on the descriptor the supervisor gave; the workload must not inherit it.
    unsafe { libc::fcntl(STATUS_FD, libc::F_SETFD, libc::FD_CLOEXEC) };
    let root = spec.root.as_str();
    let at = |path: &str| format!("{root}{path}");
    unshare(libc::CLONE_NEWNS)?;
    context(
        "private mounts",
        sys::mount("", "/", "", libc::MS_REC | libc::MS_PRIVATE, ""),
    )?;
    sys::mount_at("proc", &at("/proc"), "proc", LOCKED, "")?;
    sys::mount_at("sysfs", &at("/sys"), "sysfs", LOCKED | libc::MS_RDONLY, "")?;
    sys::mount_at("cgroup2", &at("/sys/fs/cgroup"), "cgroup2", LOCKED, "")?;
    sys::mount_at("dev", &at("/dev"), "tmpfs", libc::MS_NOSUID, "mode=0755,size=4m")?;
    sys::devices(root, &[])?;
    sys::mount_at(
        "devpts",
        &at("/dev/pts"),
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620,gid=5",
    )?;
    sys::mount_at(
        "shm",
        &at("/dev/shm"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        &format!("mode=1777,size={}", spec.shm_bytes),
    )?;

    let shared = at("/run/codefusion");
    sys::mount_at(&spec.shared, &shared, "", libc::MS_BIND, "")?;
    context(
        "read-only hooks",
        sys::mount(
            "",
            &shared,
            "",
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NOSUID | libc::MS_NODEV,
            "",
        ),
    )?;
    // The cache's folders the job writes, npm's, pnpm's and Playwright's, in this namespace only: they go with the container's
    // last process, and the supervisor can then let go of the disk. The top level and Docker's data are not the
    // job's to see. No devices or set-id programs; npx runs what it installed there, so they may execute.
    if let Some(cache) = &spec.cache {
        for folder in cf_supervisor::cache::USER_FOLDERS {
            let path = format!("{cache}/{folder}");
            sys::mount_at(&path, &at(&path), "", libc::MS_BIND, "")?;
            context(
                "the cache's options",
                sys::mount(
                    "",
                    &at(&path),
                    "",
                    libc::MS_BIND | libc::MS_REMOUNT | libc::MS_NOSUID | libc::MS_NODEV,
                    "",
                ),
            )?;
        }
    }
    // What would let root in the container reach the machine: the keys that reboot it, its memory, its settings.
    for masked in ["/proc/sysrq-trigger", "/proc/kcore"] {
        if fs::metadata(at(masked)).is_ok() {
            context(
                masked,
                sys::mount(&at("/dev/null"), &at(masked), "", libc::MS_BIND, ""),
            )?;
        }
    }
    let settings = at("/proc/sys");
    context(
        "settings",
        sys::mount(&settings, &settings, "", libc::MS_BIND | libc::MS_REC, ""),
    )?;
    context(
        "read-only settings",
        sys::mount(
            "",
            &settings,
            "",
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | LOCKED,
            "",
        ),
    )?;

    sys::pivot_into(root)?;

    // From here on root is root of this container only. A mount namespace made with the user namespace locks
    // every mount above.
    unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS)?;
    await_ids(ask, done, "container")?;

    let workload = fork()?;
    if workload == 0 {
        let error = start(spec);
        eprintln!("cf-supervisor: the workload did not start: {error}");
        std::process::exit(127);
    }
    loop {
        let (ended, status) = wait(-1);
        if ended == workload || ended < 0 {
            return Ok(status);
        }
    }
}

/// Becomes the workload. Returns only when that failed.
fn start(spec: &Spec) -> String {
    let Some(program) = spec.argv.first() else {
        return "it has no program".into();
    };
    let to_c = |text: &str| CString::new(text).map_err(|_| "a NUL byte in its arguments".to_string());
    let arguments: Result<Vec<CString>, String> = spec.argv.iter().map(|argument| to_c(argument)).collect();
    let environment: Result<Vec<CString>, String> = spec
        .env
        .iter()
        .map(|(name, value)| to_c(&format!("{name}={value}")))
        .collect();
    let (arguments, environment, program, home) =
        match (arguments, environment, to_c(program), to_c(&spec.home)) {
            (Ok(arguments), Ok(environment), Ok(program), Ok(home)) => {
                (arguments, environment, program, home)
            }
            (Err(error), ..) | (_, Err(error), ..) | (_, _, Err(error), _) | (.., Err(error)) => {
                return error;
            }
        };
    let groups: Vec<libc::gid_t> = spec.groups.clone();
    // SAFETY: every pointer is to memory that lives through the call; the calls change only this process, which has
    // one thread. Rust ignores SIGPIPE, and an ignored signal stays ignored across exec, so it is set back.
    unsafe {
        libc::setsid();
        libc::umask(0o022);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        if libc::setgroups(groups.len(), groups.as_ptr()) != 0
            || libc::setgid(spec.gid) != 0
            || libc::setuid(spec.uid) != 0
        {
            return format!("user {}: {}", spec.uid, std::io::Error::last_os_error());
        }
        if libc::chdir(home.as_ptr()) != 0 {
            libc::chdir(c"/".as_ptr());
        }
    }
    format!(
        "{}: {}",
        spec.argv[0],
        sys::exec(&program, &arguments, &environment)
    )
}

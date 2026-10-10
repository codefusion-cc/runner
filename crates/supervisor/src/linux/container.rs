//! The job's container. The supervisor starts its own binary again as the launcher; the launcher makes the
//! namespaces and their first process; that process builds the container's view of the machine, gives up real
//! root, and starts the workload as the image's user.
//!
//! What holds a job in: its processes sit in a control group whose limits are set one level above what the
//! container can see; they are root only in a user namespace made last, so every mount made before is locked,
//! `/proc/sys` and `/sys` are read-only, and the network (shared with the supervisor, so no bridge is needed) and
//! its firewall belong to a namespace their root has no power over.

use super::sys::{self, check, context};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::symlink;

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
}

/// How the container ended.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The workload's wait status, as `waitpid` gives it.
    Status(i32),
    /// The container could not be made.
    Error(String),
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

fn fork() -> Result<libc::pid_t, String> {
    // SAFETY: the launcher and the container's first process have one thread each, so the child may do anything.
    context("fork", check(unsafe { libc::fork() }))
}

fn unshare(flags: libc::c_int) -> Result<(), String> {
    // SAFETY: unshare takes no pointers.
    context("new namespaces", check(unsafe { libc::unshare(flags) }).map(drop))
}

fn wait(pid: libc::pid_t) -> (libc::pid_t, libc::c_int) {
    let mut status = 0;
    loop {
        // SAFETY: waitpid fills the status we own.
        let ended = unsafe { libc::waitpid(pid, &mut status, 0) };
        if ended > 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return (ended, status);
        }
    }
}

fn launcher() -> Result<Infallible, String> {
    let mut text = String::new();
    context(
        "read the container's description",
        std::io::stdin().read_to_string(&mut text),
    )?;
    let spec: Spec =
        serde_json::from_str(&text).map_err(|_| "the container's description is malformed".to_string())?;
    drop(text);
    if let Ok(null) = File::open("/dev/null") {
        // SAFETY: dup2 on descriptors we own.
        unsafe { libc::dup2(null.as_raw_fd(), 0) };
    }
    // The kernel's choice under memory pressure is a process of the job's, never what holds the job together.
    let _ = fs::write("/proc/self/oom_score_adj", "-1000");
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
    // The first process asks once it has its user namespace; only real root can give that namespace every id.
    let mut byte = [0u8; 1];
    let mut done = File::from(done_write);
    if File::from(ask_read).read(&mut byte).unwrap_or(0) == 1 {
        let whole = "0 0 4294967295\n";
        match fs::write(format!("/proc/{first}/uid_map"), whole)
            .and_then(|()| fs::write(format!("/proc/{first}/gid_map"), whole))
        {
            Ok(()) => drop(done.write_all(&[1])),
            Err(error) => eprintln!("cf-supervisor: the container's ids could not be set: {error}"),
        }
    }
    // Closed without the byte, this end tells the first process that it got no ids, so it does not wait.
    drop(done);
    let (_, status) = wait(first);
    std::process::exit(if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    })
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
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
    ] {
        let path = sys::cstr(&at(&format!("/dev/{name}")));
        // SAFETY: mknod and chmod take one NUL terminated path each.
        context(
            &format!("device {name}"),
            check(unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) }),
        )?;
        // SAFETY: as above; chmod, since mknod's mode passes through the process's mask.
        unsafe { libc::chmod(path.as_ptr(), 0o666) };
    }
    for (link, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
        ("ptmx", "pts/ptmx"),
    ] {
        context("device links", symlink(target, at(&format!("/dev/{link}"))))?;
    }
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

    context("enter the root", std::env::set_current_dir(root))?;
    let here = sys::cstr(".");
    // SAFETY: pivot_root and umount2 take NUL terminated paths.
    context(
        "new root",
        check(unsafe { libc::syscall(libc::SYS_pivot_root, here.as_ptr(), here.as_ptr()) } as libc::c_int),
    )?;
    // SAFETY: as above.
    context(
        "leave the old root",
        check(unsafe { libc::umount2(here.as_ptr(), libc::MNT_DETACH) }),
    )?;
    context("enter the root", std::env::set_current_dir("/"))?;

    // From here on root is root of this container only. A mount namespace made with the user namespace locks
    // every mount above.
    unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS)?;
    context("ask for ids", File::from(ask).write_all(&[1]))?;
    let mut byte = [0u8; 1];
    if File::from(done).read(&mut byte).unwrap_or(0) != 1 {
        return Err("the launcher gave the container no ids".into());
    }

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
    let _ = fs::write("/proc/self/oom_score_adj", "0");
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
    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|string| string.as_ptr())
            .chain([std::ptr::null()])
            .collect::<Vec<_>>()
    };
    let (argv, envp) = (pointers(&arguments), pointers(&environment));
    let groups: Vec<libc::gid_t> = spec.groups.clone();
    // SAFETY: every pointer is to memory that lives until exec; the calls change only this process, which has one
    // thread. Rust ignores SIGPIPE, and an ignored signal stays ignored across exec, so it is set back.
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
        libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr());
    }
    format!("{}: {}", spec.argv[0], std::io::Error::last_os_error())
}

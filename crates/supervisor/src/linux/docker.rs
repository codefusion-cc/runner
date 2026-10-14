//! A job's Docker daemon, in a sandbox beside its container (`cf_supervisor::docker` says why). Three parts, each in
//! its own process:
//!
//! - [`prepare`] and [`start`], in the supervisor before the job's container is made: the modules, the control group,
//!   the folders on the job's disk, the socket's folder in the job's root, and the launcher.
//! - [`launch`], the launcher, as the machine's root: the sandbox's process, mount, control group, UTS and IPC
//!   namespaces, its view of the job's root with an unmasked `/proc`, then a user namespace whose root is a shifted
//!   id, which the launcher gives its ids. Everything mounted before is locked from there on.
//! - [`sandbox`], the sandbox's first program, as the sandbox's root: a network namespace for the daemon, pasta to
//!   join it to the machine's network, and the daemon.

use super::init::{SELF_COPY, load_modules};
use super::net;
use super::sys::{self, check, context, say};
use cf_supervisor::docker::{self as plan, SHIFT};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::ffi::CString;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// The modules only a Docker job loads (`cargo xtask guest` lists them), and where they wait until then.
pub const MODULE_LIST: &str = "/etc/modules-docker";
pub const MODULE_DIR: &str = "/lib/modules-docker";
/// Where the supervisor's own copy is in the sandbox, for the sandbox's first program.
const SANDBOX_SELF: &str = "/run/cf-sandbox/supervisor";

/// What the launcher is told, on its input.
#[derive(Serialize, Deserialize)]
pub struct Spec {
    /// The job's root, which the sandbox sees too.
    pub root: String,
    /// The daemon's folder on the job's disk, mounted at `/var/lib/docker`.
    pub data: String,
    /// The socket's folder on the job's disk, mounted at [`plan::SOCKET_DIR`] here and in the job's container.
    pub sockets: String,
    /// The control group the sandbox sees as its root, beside the container's and under the job's limits.
    pub cgroup: String,
    /// The sandbox's host name.
    pub host_name: String,
    /// The sandbox's `uid_map` and `gid_map`.
    pub uid_map: String,
    /// See `uid_map`.
    pub gid_map: String,
    /// The size of the sandbox's `/dev/shm`.
    pub shm_bytes: u64,
}

fn chown(path: &str, uid: u32, gid: u32) -> Result<(), String> {
    context(path, std::os::unix::fs::chown(path, Some(uid), Some(gid)))
}

/// The modules, the sandbox's control group and folders, and the socket's folder in the job's root. `root` is the
/// job's root, `scratch` the job's disk, `cgroup` the job's control group; `passwd` and `group` are the image's.
/// An error says what the job is missing; the job runs without Docker and is told why.
pub fn prepare(
    root: &str,
    scratch: &str,
    cgroup: &str,
    (uid, gid): (u32, u32),
    group: &str,
    host_name: &str,
    shm_bytes: u64,
) -> Result<Spec, String> {
    load_modules(MODULE_LIST, MODULE_DIR)?;
    let _ = fs::remove_dir_all(MODULE_DIR);
    let docker_gid = cf_supervisor::environment::find_group(group, plan::GROUP)
        .ok_or_else(|| format!("the image has no group {}", plan::GROUP))?;

    // The daemon's processes get processor time limits too, where the kernel has the controller.
    for at in ["/sys/fs/cgroup".to_string(), cgroup.to_string()] {
        let _ = fs::write(format!("{at}/cgroup.subtree_control"), "+cpu");
    }
    let sandbox_group = format!("{cgroup}/docker");
    // Delegated to the sandbox's root, which makes a group for each container below it. The job's limits stay one
    // level up, where the sandbox cannot see them. The sandbox's own processes go to a leaf, `daemon`, since a group
    // with processes of its own gives its children no controllers (see `launcher`).
    for group in [sandbox_group.clone(), format!("{sandbox_group}/daemon")] {
        context("the sandbox's control group", fs::create_dir_all(&group))?;
        chown(&group, SHIFT, SHIFT)?;
        for file in ["cgroup.procs", "cgroup.threads", "cgroup.subtree_control"] {
            chown(&format!("{group}/{file}"), SHIFT, SHIFT)?;
        }
    }

    let data = format!("{scratch}/docker");
    let sockets = format!("{scratch}/cf-docker");
    for (folder, owner_group, mode) in [(&data, SHIFT, 0o700), (&sockets, docker_gid, 0o750)] {
        context(folder, fs::create_dir_all(folder))?;
        chown(folder, SHIFT, owner_group)?;
        context(
            folder,
            fs::set_permissions(folder, fs::Permissions::from_mode(mode)),
        )?;
    }
    // The socket's folder in the job's root: its container is made from this mount namespace and keeps the mount.
    let in_root = format!("{root}{}", plan::SOCKET_DIR);
    sys::mount_at(&sockets, &in_root, "", libc::MS_BIND, "")?;
    // `/run/docker.sock`, and `/var/run/docker.sock` through the image's link, where tools look first.
    let link = format!("{root}/run/docker.sock");
    let _ = fs::remove_file(&link);
    context(&link, symlink("cf-docker/docker.sock", &link))?;
    context(
        "the daemon's folder",
        fs::create_dir_all(format!("{root}{}", plan::DATA)),
    )?;

    let same = [uid, gid, docker_gid];
    let uid_map = plan::map_text(&plan::extents(&same[..1]));
    let gid_map = plan::map_text(&plan::extents(&same[1..]));
    Ok(Spec {
        root: root.into(),
        data,
        sockets,
        cgroup: sandbox_group,
        host_name: host_name.into(),
        uid_map,
        gid_map,
        shm_bytes,
    })
}

/// The descriptor the launcher says on that the sandbox's view is built.
const VIEW_FD: libc::c_int = 101;
/// How long the job's container waits for the sandbox's view.
const VIEW_WAIT_MS: libc::c_int = 10_000;

/// Starts the launcher, which makes the sandbox, and returns once the sandbox has built its view of the job's root:
/// it does so as the machine's root, so the job must not run yet, or its root could change those paths under it. A
/// launcher that does not get there in time is ended. Its end goes to the machine's console and, as why the job has
/// no daemon, to `problem`, which the job's start hook reads. What the daemon says goes to [`plan::LOG`], which the
/// job can read.
pub fn start(spec: &Spec, problem: &str) -> Result<(), String> {
    let log_path = format!("{}/dockerd.log", spec.sockets);
    let log = context(
        "the daemon's log",
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(&log_path),
    )?;
    let (view_read, view_write) = context("pipe", sys::pipe())?;
    let mut command = Command::new(SELF_COPY);
    command
        .arg("docker")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(context("the daemon's log", log.try_clone())?)
        .stderr(log);
    let view_fd = view_write.as_raw_fd();
    // SAFETY: dup2 is async-signal-safe; the descriptor is open in the child, which is a copy of this process.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(view_fd, VIEW_FD) < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = context("start the Docker launcher", command.spawn())?;
    // The command holds the end the launcher got; while it is open here, the pipe never ends.
    drop((command, view_write));
    if let Some(mut input) = child.stdin.take() {
        context(
            "describe the sandbox",
            input.write_all(serde_json::to_string(spec).unwrap_or_default().as_bytes()),
        )?;
    }
    let mut poll = libc::pollfd {
        fd: view_read.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd and a length of 1.
    unsafe { libc::poll(&mut poll, 1, VIEW_WAIT_MS) };
    let mut byte = [0u8; 1];
    let built = poll.revents & libc::POLLIN != 0 && File::from(view_read).read(&mut byte).unwrap_or(0) == 1;
    if !built {
        let _ = child.kill();
    }
    let problem = problem.to_string();
    std::thread::spawn(move || {
        if let Ok(status) = child.wait() {
            say(&format!("the Docker sandbox ended: {status}"));
            // Read by the start hook, which then stops waiting for the daemon; at the job's end nobody reads it.
            if !std::path::Path::new(&problem).exists() {
                let _ = fs::write(&problem, format!("its sandbox ended ({status})."));
            }
        }
    });
    if built {
        Ok(())
    } else {
        Err("the sandbox did not build its view in time".into())
    }
}

fn fork() -> Result<libc::pid_t, String> {
    // SAFETY: the launcher and the sandbox's first program have one thread each, so the child may do anything.
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

fn exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else {
        128 + libc::WTERMSIG(status)
    }
}

/// The launcher's entry, as the machine's root.
pub fn launch() -> ! {
    match launcher() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("cf-supervisor: the Docker sandbox was not made: {error}");
            std::process::exit(70)
        }
    }
}

fn launcher() -> Result<i32, String> {
    let mut text = String::new();
    context(
        "read the sandbox's description",
        std::io::stdin().read_to_string(&mut text),
    )?;
    let spec: Spec =
        serde_json::from_str(&text).map_err(|_| "the sandbox's description is malformed".to_string())?;
    if let Ok(null) = File::open("/dev/null") {
        // SAFETY: dup2 on descriptors we own.
        unsafe { libc::dup2(null.as_raw_fd(), 0) };
    }
    // The job's root, which shares the machine's ids, can neither trace the launcher nor open what it holds.
    // SAFETY: prctl with this option takes numbers only.
    context(
        "keep to itself",
        check(unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) }),
    )?;
    // SAFETY: the supervisor hands the launcher this descriptor, and it is wrapped once, here.
    let view = unsafe { OwnedFd::from_raw_fd(VIEW_FD) };
    // SAFETY: fcntl on the descriptor we own; the sandbox's programs must not inherit it.
    unsafe { libc::fcntl(view.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
    // The sandbox sees its group as the root of the control groups; it and its processes sit in the leaf `daemon`,
    // so the group is empty and gives the containers' groups beside the leaf memory, process and processor limits.
    sys::set(format!("{}/cgroup.procs", spec.cgroup), "0")?;
    unshare(libc::CLONE_NEWCGROUP)?;
    sys::set(format!("{}/daemon/cgroup.procs", spec.cgroup), "0")?;
    sys::set(format!("{}/cgroup.subtree_control", spec.cgroup), "+memory +pids")?;
    let _ = fs::write(format!("{}/cgroup.subtree_control", spec.cgroup), "+cpu");
    // A process namespace made by the machine's root, so that its `/proc` is mounted before the user namespace.
    unshare(libc::CLONE_NEWPID)?;
    let (ask_read, ask_write) = context("pipe", sys::pipe())?;
    let (done_read, done_write) = context("pipe", sys::pipe())?;
    let first = fork()?;
    if first == 0 {
        drop((ask_read, done_write));
        let Err(error) = first_process(&spec, view, ask_write, done_read);
        eprintln!("cf-supervisor: the Docker sandbox did not start: {error}");
        std::process::exit(70);
    }
    drop((view, ask_write, done_read));
    let mut byte = [0u8; 1];
    let mut done = File::from(done_write);
    if File::from(ask_read).read(&mut byte).unwrap_or(0) == 1 {
        match fs::write(format!("/proc/{first}/uid_map"), &spec.uid_map)
            .and_then(|()| fs::write(format!("/proc/{first}/gid_map"), &spec.gid_map))
        {
            Ok(()) => drop(done.write_all(&[1])),
            Err(error) => eprintln!("cf-supervisor: the sandbox's ids could not be set: {error}"),
        }
    }
    drop(done);
    let (_, status) = wait(first);
    Ok(exit_code(status))
}

/// The sandbox's first process, PID 1 of its process namespace: builds its view as the machine's root, says so on
/// `built`, becomes the sandbox's root and runs [`sandbox`]. Returns only when that failed.
fn first_process(spec: &Spec, built: OwnedFd, ask: OwnedFd, done: OwnedFd) -> Result<Infallible, String> {
    unshare(libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC)?;
    // SAFETY: sethostname reads `len` bytes of the string we own.
    unsafe { libc::sethostname(spec.host_name.as_ptr().cast(), spec.host_name.len()) };
    view(spec)?;
    // Nothing more is done in the job's root as the machine's root: its container may start.
    context("say the view is built", File::from(built).write_all(&[1]))?;

    // From here on root is the sandbox's root, a shifted id: what was mounted above is locked.
    unshare(libc::CLONE_NEWUSER)?;
    context("ask for ids", File::from(ask).write_all(&[1]))?;
    let mut byte = [0u8; 1];
    if File::from(done).read(&mut byte).unwrap_or(0) != 1 {
        return Err("the launcher gave the sandbox no ids".into());
    }
    // SAFETY: these calls take numbers or a null list of no groups, and change only this process.
    unsafe {
        if libc::setresgid(0, 0, 0) != 0
            || libc::setgroups(0, std::ptr::null()) != 0
            || libc::setresuid(0, 0, 0) != 0
        {
            return Err(format!(
                "become the sandbox's root: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    unshare(libc::CLONE_NEWNS)?;
    let program = sys::cstr(SANDBOX_SELF);
    let arguments = [sys::cstr(SANDBOX_SELF), sys::cstr("docker-sandbox")];
    let environment: Vec<CString> = sandbox_environment()
        .iter()
        .map(|entry| sys::cstr(entry))
        .collect();
    let pointers = |strings: &[CString]| {
        strings
            .iter()
            .map(|string| string.as_ptr())
            .chain([std::ptr::null()])
            .collect::<Vec<_>>()
    };
    let (argv, envp) = (pointers(&arguments), pointers(&environment));
    // SAFETY: every pointer is to memory that lives until exec.
    unsafe { libc::execve(program.as_ptr(), argv.as_ptr(), envp.as_ptr()) };
    Err(format!("{SANDBOX_SELF}: {}", std::io::Error::last_os_error()))
}

fn sandbox_environment() -> Vec<String> {
    vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".into(),
        "HOME=/tmp".into(),
        "LANG=C.UTF-8".into(),
    ]
}

const PLAIN: libc::c_ulong = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC;

/// The sandbox's view of the job's root, as the machine's root in the sandbox's mount namespace: an unmasked `/proc`
/// of its own process namespace and a `/sys`, both of which the kernel wants visible before the daemon's containers
/// mount theirs; its own `/dev` and `/run`, owned by the sandbox's root; the daemon's folder and the socket's. The
/// rest is the job's, `/tmp` and the workspace too, so a path a step names in `docker run -v` is the step's file.
fn view(spec: &Spec) -> Result<(), String> {
    let root = spec.root.as_str();
    let at = |path: &str| format!("{root}{path}");
    let owned = |mode: &str| format!("mode={mode},uid={SHIFT},gid={SHIFT}");
    context(
        "private mounts",
        sys::mount("", "/", "", libc::MS_REC | libc::MS_PRIVATE, ""),
    )?;
    sys::mount_at("proc", &at("/proc"), "proc", PLAIN, "")?;
    sys::mount_at("sysfs", &at("/sys"), "sysfs", PLAIN, "")?;
    sys::mount_at("cgroup2", &at("/sys/fs/cgroup"), "cgroup2", PLAIN, "")?;
    sys::mount_at("dev", &at("/dev"), "tmpfs", libc::MS_NOSUID, "mode=0755,size=4m")?;
    context("/dev/net", fs::create_dir_all(at("/dev/net")))?;
    for (name, major, minor) in [
        ("null", 1, 3),
        ("zero", 1, 5),
        ("full", 1, 7),
        ("random", 1, 8),
        ("urandom", 1, 9),
        ("tty", 5, 0),
        // pasta's and the daemon's network devices, in namespaces the sandbox owns.
        ("net/tun", 10, 200),
    ] {
        let path = sys::cstr(&at(&format!("/dev/{name}")));
        // SAFETY: mknod and chmod take one NUL terminated path each.
        context(
            &format!("device {name}"),
            check(unsafe { libc::mknod(path.as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(major, minor)) }),
        )?;
        // SAFETY: as above.
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
        "newinstance,ptmxmode=0666,mode=0620",
    )?;
    sys::mount_at(
        "shm",
        &at("/dev/shm"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        &format!("{},size={}", owned("1777"), spec.shm_bytes),
    )?;
    sys::mount_at(
        "run",
        &at("/run"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        &owned("0755"),
    )?;
    sys::mount_at(&spec.sockets, &at(plan::SOCKET_DIR), "", libc::MS_BIND, "")?;
    sys::mount_at(&spec.data, &at(plan::DATA), "", libc::MS_BIND, "")?;
    let program = at(SANDBOX_SELF);
    context("the sandbox's program", fs::create_dir_all(at("/run/cf-sandbox")))?;
    context("the sandbox's program", File::create(&program).map(drop))?;
    sys::mount_at(SELF_COPY, &program, "", libc::MS_BIND, "")?;
    context(
        "read-only program",
        sys::mount(
            "",
            &program,
            "",
            libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY | libc::MS_NODEV,
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
    context("enter the root", std::env::set_current_dir("/"))
}

/// The sandbox's first program, as its root and PID 1 of its process namespace, in the machine's network: makes the
/// daemon's network namespace, joins it to the machine's with pasta, starts the daemon in it, and reaps until the
/// daemon ends. Its output is the daemon's log.
pub fn sandbox() -> ! {
    match run_sandbox() {
        Ok(code) => std::process::exit(code),
        Err(error) => {
            eprintln!("cf-sandbox: {error}");
            std::process::exit(70)
        }
    }
}

fn run_sandbox() -> Result<i32, String> {
    let (ready_read, ready_write) = context("pipe", sys::pipe())?;
    let (go_read, go_write) = context("pipe", sys::pipe())?;
    let daemon = fork()?;
    if daemon == 0 {
        drop((ready_read, go_write));
        let Err(error) = daemon_process(ready_write, go_read);
        eprintln!("cf-sandbox: the daemon did not start: {error}");
        std::process::exit(70);
    }
    drop((ready_write, go_read));
    let mut byte = [0u8; 1];
    if File::from(ready_read).read(&mut byte).unwrap_or(0) != 1 {
        let (_, status) = wait(daemon);
        return Ok(exit_code(status));
    }
    let command = plan::pasta_command(&format!("/proc/{daemon}/ns/net"));
    // pasta configures the namespace, then stays in the background and returns here.
    let status = context(
        "start pasta",
        Command::new(&command[0])
            .args(&command[1..])
            .env_clear()
            .envs(
                sandbox_environment()
                    .iter()
                    .filter_map(|entry| entry.split_once('=')),
            )
            .stdin(Stdio::null())
            .status(),
    )?;
    if !status.success() {
        sys::kill(daemon as u32, libc::SIGKILL);
        return Err(format!("pasta ended with {status}: the daemon has no network"));
    }
    context("start the daemon", File::from(go_write).write_all(&[1]))?;
    loop {
        let (ended, status) = wait(-1);
        if ended == daemon {
            eprintln!("cf-sandbox: the daemon ended with {}", exit_code(status));
            return Ok(exit_code(status));
        }
        if ended < 0 {
            return Ok(70);
        }
    }
}

/// The daemon's process: a network namespace of the sandbox's, its loopback up, then, once pasta has joined the
/// namespace to the machine's network, its one rule ([`plan::flagless_tcp_rule`]) and the daemon. Returns only when
/// that failed.
fn daemon_process(ready: OwnedFd, go: OwnedFd) -> Result<Infallible, String> {
    unshare(libc::CLONE_NEWNET)?;
    net::loopback_up()?;
    context("say ready", File::from(ready).write_all(&[1]))?;
    let mut byte = [0u8; 1];
    if File::from(go).read(&mut byte).unwrap_or(0) != 1 {
        return Err("the daemon's network was not joined".into());
    }
    let rule = plan::flagless_tcp_rule();
    let status = context(
        "the daemon's network rule",
        Command::new(&rule[0])
            .args(&rule[1..])
            .env_clear()
            .envs(
                sandbox_environment()
                    .iter()
                    .filter_map(|entry| entry.split_once('=')),
            )
            .stdin(Stdio::null())
            .status(),
    )?;
    if !status.success() {
        return Err(format!(
            "the daemon's network rule: {} ended with {status}",
            rule[0]
        ));
    }
    let command = plan::dockerd_command();
    let error = Command::new(&command[0])
        .args(&command[1..])
        .env_clear()
        .envs(
            sandbox_environment()
                .iter()
                .filter_map(|entry| entry.split_once('=')),
        )
        .stdin(Stdio::null())
        .exec();
    Err(format!("{}: {error}", command[0]))
}

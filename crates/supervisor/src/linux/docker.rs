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

use super::container::{await_ids, give_ids, read_spec};
use super::init::{SELF_COPY, load_modules, spawn_self};
use super::job::{disk, image_tool, mke2fs_args};
use super::net;
use super::sys::{self, check, context, exit_code, fork, say, unshare, wait};
use cf_guest_proto::{CACHE_DISK, CacheEnd, CacheFound};
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
/// job's root, `scratch` the job's disk, `cgroup` the job's control group; the job user's ids come with the image's
/// `/etc/group`. With a cache
/// mounted ([`mount_cache`]) the daemon keeps its data there instead of on the job's disk.
/// An error says what the job is missing; the job runs without Docker and is told why.
pub fn prepare(
    root: &str,
    scratch: &str,
    cgroup: &str,
    (uid, gid, group): (u32, u32, &str),
    host_name: &str,
    shm_bytes: u64,
    cache: Option<CacheFound>,
) -> Result<Spec, String> {
    load_modules(MODULE_LIST, MODULE_DIR)?;
    let _ = fs::remove_dir_all(MODULE_DIR);
    let docker_gid = cf_supervisor::environment::find_group(group, plan::GROUP)
        .ok_or_else(|| format!("the image has no group {}", plan::GROUP))?;

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

    let data = match cache {
        Some(_) => format!("{CACHE}/docker"),
        None => format!("{scratch}/docker"),
    };
    let sockets = format!("{scratch}/sockets");
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
    context(&link, symlink(plan::SOCKET, &link))?;
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
    let log = context(
        "the daemon's log",
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o644)
            .open(format!("{}/{}", spec.sockets, plan::LOG_NAME)),
    )?;
    let (view_read, view_write) = context("pipe", sys::pipe())?;
    let mut child = spawn_self(
        "docker",
        (&view_write, VIEW_FD),
        (
            Stdio::from(context("the daemon's log", log.try_clone())?),
            Stdio::from(log),
        ),
        spec,
    )?;
    // While the end the launcher got is open here too, the pipe never ends.
    drop(view_write);
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
    let spec: Spec = read_spec("sandbox")?;
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
    give_ids(
        first,
        ask_read,
        done_write,
        (&spec.uid_map, &spec.gid_map),
        "sandbox",
    );
    let (_, status) = wait(first);
    Ok(exit_code(status))
}

/// The sandbox's first process, PID 1 of its process namespace: builds its view as the machine's root, says so on
/// `built`, becomes the sandbox's root and runs [`sandbox`]. Returns only when that failed.
fn first_process(spec: &Spec, built: OwnedFd, ask: OwnedFd, done: OwnedFd) -> Result<Infallible, String> {
    // A launcher ended for being too slow takes this process with it, so nothing builds the view as the machine's
    // root once the job's container starts. Being PID 1 of its namespace, its end takes the whole sandbox too.
    // SAFETY: prctl with this option takes numbers only.
    context(
        "end with the launcher",
        check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) }),
    )?;
    unshare(libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC)?;
    // SAFETY: sethostname reads `len` bytes of the string we own.
    unsafe { libc::sethostname(spec.host_name.as_ptr().cast(), spec.host_name.len()) };
    view(spec)?;
    // Nothing more is done in the job's root as the machine's root: its container may start.
    context("say the view is built", File::from(built).write_all(&[1]))?;

    // From here on root is the sandbox's root, a shifted id: what was mounted above is locked.
    unshare(libc::CLONE_NEWUSER)?;
    await_ids(ask, done, "sandbox")?;
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
    let environment: Vec<CString> = SANDBOX_ENV
        .iter()
        .map(|(name, value)| sys::cstr(&format!("{name}={value}")))
        .collect();
    let error = sys::exec(
        &sys::cstr(SANDBOX_SELF),
        &[sys::cstr(SANDBOX_SELF), sys::cstr("docker-sandbox")],
        &environment,
    );
    Err(format!("{SANDBOX_SELF}: {error}"))
}

/// The whole environment of the sandbox's programs.
const SANDBOX_ENV: [(&str, &str); 3] = [
    (
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    ),
    ("HOME", "/tmp"),
    ("LANG", "C.UTF-8"),
];

/// One of the sandbox's programs, `argv`, with the sandbox's environment and nothing on its input.
fn sandbox_command(argv: &[String]) -> Command {
    let mut command = Command::new(&argv[0]);
    command
        .args(&argv[1..])
        .env_clear()
        .envs(SANDBOX_ENV)
        .stdin(Stdio::null());
    command
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
    // pasta's and the daemon's network devices, in namespaces the sandbox owns.
    sys::devices(root, &[("net/tun", 10, 200)])?;
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
    sys::pivot_into(root)
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
    let status = context("start pasta", sandbox_command(&command).status())?;
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

/// The daemon's process: a network namespace of the sandbox's, its loopback up and its one rule
/// ([`plan::flagless_tcp_rule`]), then the daemon once pasta has joined the namespace to the machine's network.
/// Returns only when that failed.
fn daemon_process(ready: OwnedFd, go: OwnedFd) -> Result<Infallible, String> {
    unshare(libc::CLONE_NEWNET)?;
    net::loopback_up()?;
    let rule = plan::flagless_tcp_rule();
    let status = context("the daemon's network rule", sandbox_command(&rule).status())?;
    if !status.success() {
        return Err(format!(
            "the daemon's network rule: {} ended with {status}",
            rule[0]
        ));
    }
    context("say ready", File::from(ready).write_all(&[1]))?;
    let mut byte = [0u8; 1];
    if File::from(go).read(&mut byte).unwrap_or(0) != 1 {
        return Err("the daemon's network was not joined".into());
    }
    let command = plan::dockerd_command();
    let error = sandbox_command(&command).exec();
    Err(format!("{}: {error}", command[0]))
}

/// Where a Docker job's cache disk is mounted, in the supervisor's view; the sandbox sees its `docker` folder as
/// [`plan::DATA`].
pub const CACHE: &str = "/mnt/cache";

/// Mounts the cache disk at [`CACHE`] and says what it held. A disk the image's `e2fsck` cannot make whole, or that
/// does not mount, is made anew: the job runs with an empty cache rather than none.
pub fn mount_cache() -> Result<CacheFound, String> {
    let device = disk(CACHE_DISK)?;
    let flags = libc::MS_NOATIME | libc::MS_NODEV | libc::MS_NOSUID;
    context("the cache's folder", fs::create_dir_all(CACHE))?;
    if !empty_disk(&device) {
        // `-p` repairs what it safely can and says 4 or more for what it cannot.
        let checked = image_tool(&["e2fsck".into(), "-p".into(), device.clone()])
            .is_ok_and(|status| status.code().is_some_and(|code| code < 4));
        if checked && sys::mount(&device, CACHE, "ext4", flags, "").is_ok() {
            return Ok(CacheFound::Reused);
        }
        say("the cache disk does not mount: it is made anew");
    }
    let found = if empty_disk(&device) {
        CacheFound::Fresh
    } else {
        CacheFound::Replaced
    };
    let status = image_tool(&mke2fs_args(&device, true))?;
    if !status.success() {
        return Err(format!("the image's mke2fs ended with {status}"));
    }
    sys::mount_at(&device, CACHE, "ext4", flags, "")?;
    Ok(found)
}

/// Whether a disk's first megabyte is all zeros: a file the host made empty, never formatted.
fn empty_disk(device: &str) -> bool {
    let mut start = vec![0u8; 1 << 20];
    File::open(device)
        .and_then(|mut file| file.read_exact(&mut start))
        .is_ok_and(|()| start.iter().all(|byte| *byte == 0))
}

/// How long the daemon gets to stop by itself at the job's end: its containers are gone by then (the job's end
/// hook removed them), so it only writes down its state.
const DAEMON_STOP: std::time::Duration = std::time::Duration::from_secs(15);
/// How long the cache disk may stay busy once every process of the sandbox is gone.
const LET_GO: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether a Docker job's daemon still answers on its socket, in the folder `sockets`.
pub fn daemon_answers(sockets: &str) -> bool {
    std::os::unix::net::UnixStream::connect(format!("{sockets}/{}", plan::SOCKET_NAME)).is_ok()
}

/// At a Docker job's end, before the job's processes are stopped: asks the daemon to stop and waits for it. Says
/// whether it stopped by itself. `sandbox` is the sandbox's control group.
pub fn stop_daemon(sandbox: &str) -> bool {
    let daemons = || -> Vec<u32> {
        fs::read_to_string(format!("{sandbox}/daemon/cgroup.procs"))
            .unwrap_or_default()
            .lines()
            .filter_map(|pid| pid.trim().parse().ok())
            .filter(|pid| {
                fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|comm| comm.trim() == "dockerd")
            })
            .collect()
    };
    for pid in daemons() {
        sys::kill(pid, libc::SIGTERM);
    }
    let deadline = std::time::Instant::now() + DAEMON_STOP;
    while !daemons().is_empty() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    true
}

/// Once every process of the job is gone: unmounts the cache, which the sandbox's namespaces held until they went.
/// Says whether it let go; a disk still mounted is not kept.
pub fn let_go_of_cache() -> bool {
    let path = sys::cstr(CACHE);
    let deadline = std::time::Instant::now() + LET_GO;
    loop {
        // SAFETY: umount2 takes a NUL terminated path.
        if unsafe { libc::umount2(path.as_ptr(), 0) } == 0 {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            say(&format!(
                "the cache disk stays busy: {}",
                std::io::Error::last_os_error()
            ));
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// What a job's cache ends as: found as `found`, kept only when the daemon stopped by itself and the disk was let go.
pub fn cache_end(found: CacheFound, (answers, stopped): (bool, bool), let_go: bool) -> CacheEnd {
    CacheEnd {
        found,
        clean: stopped && let_go,
        daemon: answers,
    }
}

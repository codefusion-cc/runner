//! One job: its disks, its limits, its container, and the watch over it until it ends.

use super::container::{Outcome, STATUS_FD, Spec};
use super::init::{Event, Failure, Host, SELF_COPY};
use super::net::Network;
use super::sys::{self, context, say};
use cf_guest_proto::{
    GuestEnd, GuestMessage, HostMessage, Job, OomEvent, Process as Used, StopReason, Summary, Usage, Workload,
};
use cf_supervisor::environment::{find_user, groups_of, workload_environment};
use cf_supervisor::oom::{self, Kill};
use cf_supervisor::processes::{Process, Table, parse_stat};
use cf_supervisor::tell::{self, JobFacts};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const IMAGE: &str = "/mnt/image";
const SCRATCH: &str = "/mnt/scratch";
const ROOT: &str = "/mnt/root";
const SHARED: &str = "/run/cf";
/// The limits are set here; the container sees only the group below, so its root cannot lift them.
const CGROUP: &str = "/sys/fs/cgroup/job";
const USER: &str = "runner";
/// How often the job is looked at.
const TICK: Duration = Duration::from_millis(500);
const HEARTBEAT: Duration = Duration::from_secs(2);
/// How much of the workload's own output the console log takes; the rest is read and dropped.
const OUTPUT_CAP: usize = 4 << 20;

/// A summary of a task that ran no job.
pub fn empty_summary(exit_code: Option<i32>) -> Summary {
    Summary {
        end: GuestEnd::Exited,
        exit_code,
        signal: None,
        usage: usage(),
        disk_peak_bytes: 0,
        oom: None,
        wall_ms: sys::uptime_ms(),
    }
}

/// The device of the disk the host attached under this name.
fn disk(id: &str) -> Result<String, String> {
    for _ in 0..500 {
        for entry in fs::read_dir("/sys/block").into_iter().flatten().flatten() {
            let serial = fs::read_to_string(entry.path().join("serial")).unwrap_or_default();
            if serial.trim() == id {
                return Ok(format!("/dev/{}", entry.file_name().to_string_lossy()));
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Err(format!("the machine has no disk {id}"))
}

fn mount_image() -> Result<(), String> {
    sys::mount_at(
        &disk("cf-image")?,
        IMAGE,
        "erofs",
        libc::MS_RDONLY | libc::MS_NODEV,
        "",
    )
}

/// Makes the file system of a job's disk, with the image's own tool. The host keeps the result and gives every
/// job a copy.
pub fn format_scratch() -> Result<Summary, String> {
    mount_image()?;
    let device = disk("cf-scratch")?;
    sys::mount_at("devtmpfs", &format!("{IMAGE}/dev"), "devtmpfs", 0, "")?;
    let mut command = Command::new("/usr/sbin/mke2fs");
    command
        .args([
            "-q",
            "-F",
            "-t",
            "ext4",
            "-O",
            "^has_journal",
            "-m",
            "0",
            "-E",
            "lazy_itable_init=1,nodiscard,root_owner=0:0",
        ])
        .arg(&device)
        .env_clear()
        .env("PATH", "/usr/sbin:/usr/bin:/sbin:/bin")
        .stdin(Stdio::null());
    let image = sys::cstr(IMAGE);
    // SAFETY: chroot and chdir are async-signal-safe and read strings that live as long as the command.
    unsafe {
        command.pre_exec(move || {
            if libc::chroot(image.as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let status = context("the image's mke2fs", command.status())?;
    if !status.success() {
        return Err(format!("the image's mke2fs ended with {status}"));
    }
    Ok(empty_summary(status.code()))
}

fn usage() -> Usage {
    let cpu = fs::read_to_string(format!("{CGROUP}/cpu.stat")).unwrap_or_default();
    Usage {
        memory_bytes: sys::number(format!("{CGROUP}/memory.current")),
        memory_peak_bytes: sys::number(format!("{CGROUP}/memory.peak")),
        cpu_usec: cpu
            .lines()
            .find_map(|line| line.strip_prefix("usage_usec "))
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0),
        disk_bytes: sys::disk_used(SCRATCH),
        pids: sys::number(format!("{CGROUP}/pids.current")) as u32,
    }
}

fn limits(job: &Job) -> Result<(), String> {
    sys::set("/sys/fs/cgroup/cgroup.subtree_control", "+memory +pids")?;
    for (group, delegate) in [(CGROUP.to_string(), true), (format!("{CGROUP}/run"), false)] {
        context("the job's control group", fs::create_dir_all(&group))?;
        sys::set(
            format!("{group}/memory.max"),
            &job.limits.memory_bytes.to_string(),
        )?;
        sys::set(format!("{group}/pids.max"), &job.limits.pids.to_string())?;
        // The kernel has the file only when it was built with swap; the guest has none either way.
        let _ = fs::write(format!("{group}/memory.swap.max"), "0");
        if delegate {
            sys::set(format!("{group}/cgroup.subtree_control"), "+memory +pids")?;
        }
    }
    Ok(())
}

/// What a job's tools expect of a machine and a container's root cannot set for itself.
fn settings() {
    for (path, value) in [
        ("net/ipv4/ping_group_range", "0 2147483647"),
        ("net/ipv4/ip_unprivileged_port_start", "0"),
        ("fs/inotify/max_user_watches", "1048576"),
        ("fs/inotify/max_user_instances", "8192"),
        ("vm/max_map_count", "262144"),
    ] {
        let _ = fs::write(format!("/proc/sys/{path}"), value);
    }
}

fn host_name(job: &Job) -> String {
    let name: String = job
        .name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(63)
        .collect();
    if name.trim_matches('-').is_empty() {
        "codefusion".to_string()
    } else {
        name.to_ascii_lowercase()
    }
}

fn write_root(path: &str, text: &str) -> Result<(), String> {
    let path = format!("{ROOT}{path}");
    // An image may have a link here; the file must be the container's own.
    let _ = fs::remove_file(&path);
    context(&path, fs::write(&path, text))
}

fn share(name: &str, text: &str, mode: u32) -> Result<(), String> {
    let path = format!("{SHARED}/{name}");
    let temporary = format!("{path}.new");
    context(
        &path,
        fs::write(&temporary, text)
            .and_then(|()| fs::set_permissions(&temporary, fs::Permissions::from_mode(mode)))
            .and_then(|()| fs::rename(&temporary, &path)),
    )
}

/// Runs a job to its end.
pub fn run(
    job: Job,
    network: JoinHandle<Result<Network, String>>,
    host: &Host,
    sender: Sender<Event>,
    events: &Receiver<Event>,
) -> Result<Summary, Failure> {
    let disks = |error| ("disks", error);
    let began = sys::uptime_ms();
    mount_image().map_err(disks)?;
    let scratch = disk("cf-scratch").map_err(disks)?;
    // The disk's size as the job asked for it; the file system on it holds a little less.
    let disk_bytes = sys::number(format!("/sys/block/{}/size", scratch.trim_start_matches("/dev/"))) * 512;
    sys::mount_at(&scratch, SCRATCH, "ext4", libc::MS_NOATIME, "noinit_itable").map_err(disks)?;
    for directory in ["upper", "work"] {
        context(
            "the job's disk",
            fs::create_dir_all(format!("{SCRATCH}/{directory}")),
        )
        .map_err(disks)?;
    }
    sys::mount_at(
        "overlay",
        ROOT,
        "overlay",
        0,
        &format!("lowerdir={IMAGE},upperdir={SCRATCH}/upper,workdir={SCRATCH}/work"),
    )
    .map_err(disks)?;

    let disks_ms = sys::uptime_ms() - began;
    let network = network
        .join()
        .unwrap_or_else(|_| Err("the network thread stopped".into()))
        .map_err(|error| ("network", error))?;
    let [a, b, c, d] = network.address;
    say(&format!(
        "network up in {} ms, address {a}.{b}.{c}.{d}",
        network.took.as_millis()
    ));

    let container = |error| ("container", error);
    let name = host_name(&job);
    let servers: String = network
        .name_servers
        .iter()
        .map(|[a, b, c, d]| format!("nameserver {a}.{b}.{c}.{d}\n"))
        .collect();
    write_root("/etc/resolv.conf", &servers).map_err(container)?;
    write_root("/etc/hostname", &format!("{name}\n")).map_err(container)?;
    write_root(
        "/etc/hosts",
        &format!("127.0.0.1 localhost\n127.0.1.1 {name}\n::1 localhost ip6-localhost ip6-loopback\n"),
    )
    .map_err(container)?;
    // SAFETY: sethostname reads `len` bytes of the string we own.
    unsafe { libc::sethostname(name.as_ptr().cast(), name.len()) };
    limits(&job).map_err(|error| ("limits", error))?;
    settings();

    let facts = JobFacts {
        host: job.host.clone(),
        limits: job.limits,
        disk_bytes,
    };
    share(
        "job.json",
        &serde_json::to_string(&facts).unwrap_or_default(),
        0o644,
    )
    .map_err(container)?;
    for hook in ["job-started", "job-completed"] {
        share(
            &format!("{hook}.sh"),
            &format!("#!/bin/sh\nexec /run/codefusion/supervisor hook {hook}\n"),
            0o755,
        )
        .map_err(container)?;
    }

    let passwd = fs::read_to_string(format!("{ROOT}/etc/passwd")).unwrap_or_default();
    let (uid, gid, home) =
        find_user(&passwd, USER).ok_or_else(|| container(format!("the image has no user {USER}")))?;
    let groups = groups_of(
        &fs::read_to_string(format!("{ROOT}/etc/group")).unwrap_or_default(),
        USER,
        gid,
    );
    let argv = match &job.workload {
        Workload::ActionsRunner { jit_config } => vec![
            format!("{home}/run.sh"),
            "--jitconfig".to_string(),
            jit_config.expose().to_string(),
        ],
        Workload::Command { argv } => argv.clone(),
    };
    let spec = Spec {
        root: ROOT.into(),
        shared: SHARED.into(),
        cgroup: format!("{CGROUP}/run"),
        argv,
        env: workload_environment(&home, USER, &job.env),
        uid,
        gid,
        groups,
        home,
        shm_bytes: job.limits.shm_bytes,
    };

    let kernel_log = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open("/dev/kmsg")
        .ok();
    if let Some(log) = &kernel_log {
        // SAFETY: lseek takes no pointers. Only what the kernel says from now on is about this job.
        unsafe { libc::lseek(log.as_raw_fd(), 0, libc::SEEK_END) };
    }
    let (launcher, status) = launch(&spec, sender).map_err(container)?;
    drop(spec);
    let ready = sys::uptime_ms();
    host.send(&GuestMessage::Ready { boot_ms: ready });
    say(&format!(
        "workload started {ready} ms after the kernel: supervisor at {began} ms, disks {disks_ms} ms, network {} ms",
        network.took.as_millis()
    ));

    // SAFETY: sysconf takes no pointers.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut watch = Watch {
        job: &job,
        host,
        launcher,
        started: Instant::now(),
        kernel_log,
        log: oom::Log::new(page),
        previous: Table::new([]),
        oom: None,
        kills_seen: 0,
        unexplained: 0,
        step: None,
        stop: None,
        killed: false,
        disk_peak: 0,
        heartbeat: Instant::now(),
    };
    let ended = watch.until_the_end(events);
    let summary_usage = usage();
    let _ = fs::write(format!("{CGROUP}/cgroup.kill"), "1");

    let mut outcome = String::new();
    let _ = File::from(status).read_to_string(&mut outcome);
    let (exit_code, signal) = match serde_json::from_str::<Outcome>(&outcome) {
        Ok(Outcome::Error(error)) => return Err(container(error)),
        Ok(Outcome::Status(status)) if libc::WIFEXITED(status) => (Some(libc::WEXITSTATUS(status)), None),
        Ok(Outcome::Status(status)) => (None, Some(libc::WTERMSIG(status))),
        // The container went before it could say: the launcher's own end is what there is.
        Err(_) => (ended.code(), ended.signal()),
    };
    Ok(Summary {
        end: watch
            .stop
            .map_or(GuestEnd::Exited, |(reason, _)| GuestEnd::Stopped(reason)),
        exit_code,
        signal,
        disk_peak_bytes: watch.disk_peak.max(summary_usage.disk_bytes),
        usage: summary_usage,
        oom: watch.oom,
        wall_ms: watch.started.elapsed().as_millis() as u64,
    })
}

/// Starts the launcher: its id, and where it reports how the container ended.
fn launch(spec: &Spec, sender: Sender<Event>) -> Result<(u32, std::os::fd::OwnedFd), String> {
    let (status_read, status_write) = context("pipe", sys::pipe())?;
    let (output_read, output_write) = context("pipe", sys::pipe())?;
    let mut command = Command::new(SELF_COPY);
    command
        .arg("container")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(context("pipe", output_write.try_clone())?)
        .stderr(output_write);
    let status_fd = status_write.as_raw_fd();
    // SAFETY: dup2 is async-signal-safe; the descriptor is open in the child, which is a copy of this process.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(status_fd, STATUS_FD) < 0 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = context("start the launcher", command.spawn())?;
    // The command holds the ends the launcher got; while they are open here, neither pipe ever ends.
    drop((command, status_write));
    let launcher = child.id();
    if let Some(mut input) = child.stdin.take() {
        context(
            "describe the container",
            input.write_all(serde_json::to_string(spec).unwrap_or_default().as_bytes()),
        )?;
    }
    std::thread::spawn(move || {
        let mut output = File::from(output_read);
        let (mut buffer, mut written) = (vec![0u8; 65536], 0usize);
        while let Ok(read @ 1..) = output.read(&mut buffer) {
            if written < OUTPUT_CAP {
                let _ = std::io::stderr().write_all(&buffer[..read]);
                written += read;
                if written >= OUTPUT_CAP {
                    say("the workload's output is cut here");
                }
            }
        }
    });
    std::thread::spawn(move || {
        if let Ok(status) = child.wait() {
            let _ = sender.send(Event::Ended(status));
        }
    });
    Ok((launcher, status_read))
}

fn processes() -> Table {
    Table::new(
        fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let pid = name.to_str()?;
                pid.bytes()
                    .all(|byte| byte.is_ascii_digit())
                    .then(|| fs::read_to_string(format!("/proc/{pid}/stat")).ok())
                    .flatten()
                    .and_then(|stat| parse_stat(&stat))
            }),
    )
}

struct Watch<'a> {
    job: &'a Job,
    host: &'a Host,
    launcher: u32,
    started: Instant,
    kernel_log: Option<File>,
    log: oom::Log,
    /// The processes at the last look, where one the kernel has since stopped is still found.
    previous: Table,
    oom: Option<OomEvent>,
    /// Stops the kernel counted for the job, and looks in a row at which its log explained fewer.
    kills_seen: u64,
    unexplained: u32,
    /// The step whose processes go when its time to fail by itself is over.
    step: Option<(Instant, Process)>,
    stop: Option<(StopReason, Instant)>,
    killed: bool,
    disk_peak: u64,
    heartbeat: Instant,
}

impl Watch<'_> {
    fn until_the_end(&mut self, events: &Receiver<Event>) -> std::process::ExitStatus {
        let mut looked = Instant::now();
        loop {
            match events.recv_timeout(TICK.saturating_sub(looked.elapsed())) {
                Ok(Event::Ended(status)) => return status,
                Ok(Event::Host(HostMessage::Stop(stop))) => {
                    self.stop(stop.reason, Duration::from_millis(stop.grace_ms))
                }
                Ok(Event::Host(HostMessage::Start(_))) => {}
                Ok(Event::HostGone) | Err(RecvTimeoutError::Disconnected) => {
                    // Nobody is left to tell how the job ends, and nothing of it may outlive its host.
                    say("the host is gone");
                    let _ = fs::write(format!("{CGROUP}/cgroup.kill"), "1");
                    sys::power_off();
                }
                Err(RecvTimeoutError::Timeout) => {}
            }
            if looked.elapsed() >= TICK {
                looked = Instant::now();
                self.look();
            }
        }
    }

    fn stop(&mut self, reason: StopReason, grace: Duration) {
        let deadline = Instant::now() + grace;
        if let Some((_, existing)) = &mut self.stop {
            *existing = (*existing).min(deadline);
            return;
        }
        self.stop = Some((reason, deadline));
        let _ = share(
            "stop.json",
            &serde_json::to_string(&reason).unwrap_or_default(),
            0o644,
        );
        say(&tell::stopped(reason, &self.job.host));
        if let Some(workload) = processes().workload(self.launcher) {
            sys::kill(workload.pid, libc::SIGTERM);
        }
    }

    fn look(&mut self) {
        let table = processes();
        for kill in self.kernel_kills() {
            self.out_of_memory(kill, &table);
        }
        if self.step.as_ref().is_some_and(|(at, _)| Instant::now() >= *at)
            && let Some((_, step)) = self.step.take()
            && table.get(step.pid).is_some_and(|now| now.started == step.started)
        {
            say(&format!(
                "stopping the step that ran out of memory ({}, process {})",
                step.name, step.pid
            ));
            for pid in table.tree(step.pid) {
                sys::kill(pid, libc::SIGKILL);
            }
        }
        if !self.killed && self.stop.is_some_and(|(_, deadline)| Instant::now() >= deadline) {
            self.killed = true;
            let _ = fs::write(format!("{CGROUP}/cgroup.kill"), "1");
        }
        if self.heartbeat.elapsed() >= HEARTBEAT {
            self.heartbeat = Instant::now();
            let usage = usage();
            self.disk_peak = self.disk_peak.max(usage.disk_bytes);
            self.host.send(&GuestMessage::Heartbeat(usage));
        }
        self.previous = table;
    }

    /// The stops the kernel made since the last look: from its log, which names the process, and when the log
    /// fell behind for two looks, from the group's counter, which does not.
    fn kernel_kills(&mut self) -> Vec<Kill> {
        let mut kills = Vec::new();
        if let Some(log) = &mut self.kernel_log {
            let mut record = [0u8; 8192];
            loop {
                match log.read(&mut record) {
                    Ok(read @ 1..) => {
                        if let Some(kill) = oom::message_of(&String::from_utf8_lossy(&record[..read]))
                            .and_then(|message| self.log.line(message))
                        {
                            kills.push(kill);
                        }
                    }
                    // The log dropped records we were too slow for; the next read goes on after them.
                    Err(error) if error.raw_os_error() == Some(libc::EPIPE) => {}
                    _ => break,
                }
            }
        }
        self.kills_seen += kills.len() as u64;
        let counted =
            oom::kills_in_events(&fs::read_to_string(format!("{CGROUP}/memory.events")).unwrap_or_default());
        if counted > self.kills_seen {
            self.unexplained += 1;
            if self.unexplained >= 2 {
                self.kills_seen = counted;
                kills.push(Kill {
                    process: Used {
                        pid: 0,
                        name: "a process".into(),
                        memory_bytes: 0,
                    },
                    largest: Vec::new(),
                });
            }
        } else {
            self.unexplained = 0;
            self.kills_seen = self.kills_seen.max(counted);
        }
        kills
    }

    fn out_of_memory(&mut self, kill: Kill, table: &Table) {
        say(&format!(
            "out of memory: the kernel stopped {} (process {})",
            kill.process.name, kill.process.pid
        ));
        if self.step.is_none() {
            let step = self
                .previous
                .step_of(kill.process.pid, self.launcher)
                .or_else(|| table.step_of(kill.process.pid, self.launcher))
                // A process that lived and died between two looks is in neither table.
                .or_else(|| table.only_step(self.launcher));
            if step.is_none() {
                say("no step to stop for it: its process is gone and no single step runs");
            }
            self.step = step.map(|step| {
                (
                    Instant::now() + Duration::from_millis(self.job.oom_grace_ms),
                    step.clone(),
                )
            });
        }
        if self.oom.is_none() {
            let event = OomEvent {
                process: kill.process,
                limit_bytes: self.job.limits.memory_bytes,
                largest: kill.largest,
            };
            let _ = share(
                "oom.json",
                &serde_json::to_string(&event).unwrap_or_default(),
                0o644,
            );
            self.host.send(&GuestMessage::Oom(event.clone()));
            self.oom = Some(event);
        }
    }
}

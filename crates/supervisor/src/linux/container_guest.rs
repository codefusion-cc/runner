//! The supervisor in a container of a Docker-compatible engine: the Runner's container mode (`container-guest`).
//!
//! The engine is the sandbox here: a container of the job's own, with the memory, CPU and process caps set on it by
//! the host, every capability dropped and no new privileges. This process is the container's first, as the image's
//! user `runner`, never root. It talks to the host on its standard input and output (the host attached to them
//! before the container started) and writes what it says, and the workload's output, to its standard error, which
//! the host keeps as the job's console log.
//!
//! What a machine's supervisor does with disks, modules, the network and a firewall is the engine's here, so this
//! runs the job and watches it as [`super::job`] does, and refuses what it cannot hold in: Docker jobs, which would
//! need a daemon of their own, and trusted jobs, which stay on machines.

use super::init::{Event, Failure, Host, LAST_WORDS, describe};
use super::job::{Sandbox, Watch, empty_summary, forward_output, share_in, usage_of};
use super::sys::{self, context, say};
use cf_guest_proto::{GuestEnd, OomEvent};
use cf_guest_proto::{
    GuestMessage, HostMessage, Job, PROTOCOL, Start, Summary, Task, Workload, read_message,
};
use cf_supervisor::environment::{find_user, workload_environment};
use cf_supervisor::oom;
use cf_supervisor::processes::{Table, Tree};
use cf_supervisor::tell::JobFacts;
use std::fs::{self, File};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;

/// The folder the workload finds its hooks and the job's facts in, as in a machine's container. The host made it,
/// the job's user's, before the container started.
const SHARED: &str = "/run/codefusion";
/// The container's own control group: with a control group namespace of its own, the container's root.
const CGROUP: &str = "/sys/fs/cgroup";
const USER: &str = "runner";

/// When this process started: a container has no boot of its own to count from.
static STARTED: OnceLock<Instant> = OnceLock::new();

fn since_start_ms() -> u64 {
    STARTED.get().map_or(0, |at| at.elapsed().as_millis() as u64)
}

/// Every process of the job, at once: everything this user may signal but this process, which as the container's
/// first is spared, and whose end ends the rest.
fn kill_all() {
    // SAFETY: kill takes no pointers.
    unsafe { libc::kill(-1, libc::SIGKILL) };
}

/// Nobody is left to tell how the job ends: the container's first process ends, and the kernel ends the rest with it.
fn end_container() -> ! {
    kill_all();
    std::process::exit(0)
}

/// A copy of a standard descriptor that no program this process starts inherits.
fn own_copy(fd: libc::c_int) -> std::io::Result<File> {
    // SAFETY: fcntl takes no pointers; the new descriptor is ours alone and wrapped once.
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copy < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `copy` is a descriptor this process just made and owns.
    Ok(File::from(unsafe { OwnedFd::from_raw_fd(copy) }))
}

/// The container's first program.
pub fn run() -> ! {
    let _ = STARTED.set(Instant::now());
    std::panic::set_hook(Box::new(|panic| {
        say(&format!("stopped by a mistake in the supervisor: {panic}"));
        end_container();
    }));
    // The job runs as this same user: without this it could open this process's descriptors through /proc and
    // speak to the host in the supervisor's name.
    // SAFETY: prctl with integer arguments only.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
    let (Ok(to_host), Ok(mut from_host)) = (own_copy(1), own_copy(0)) else {
        say("no host on standard input and output");
        end_container();
    };
    // The workload gets neither: its standard input is empty and its output goes through the supervisor.
    for fd in [0, 1] {
        // SAFETY: the null device is opened and put in the place of a standard descriptor; no pointer outlives it.
        unsafe {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            if null >= 0 {
                libc::dup2(null, fd);
                libc::close(null);
            }
        }
    }
    let host = Host::new(to_host);
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string();
    host.send(&GuestMessage::Hello {
        protocol: PROTOCOL,
        kernel,
    });
    let start = match read_message::<HostMessage>(&mut from_host) {
        Ok(Some(HostMessage::Start(start))) => start,
        other => {
            say(&format!("the host did not start a task: {}", describe(&other)));
            end_container();
        }
    };
    let (sender, events) = mpsc::channel();
    let to_watch = sender.clone();
    std::thread::spawn(move || {
        loop {
            match read_message::<HostMessage>(&mut from_host) {
                Ok(Some(message)) => {
                    if to_watch.send(Event::Host(message)).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = to_watch.send(Event::HostGone);
                    return;
                }
            }
        }
    });
    match perform(start, &host, sender, &events) {
        Ok(summary) => host.send(&GuestMessage::Exited(summary)),
        Err((stage, message)) => {
            say(&format!("{stage}: {message}"));
            host.send(&GuestMessage::Failed {
                stage: stage.to_string(),
                message,
            });
        }
    }
    let deadline = Instant::now() + LAST_WORDS;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(Event::HostGone) | Err(_) => break,
            Ok(_) => {}
        }
    }
    say("the container ends");
    end_container()
}

fn perform(
    start: Start,
    host: &Host,
    sender: Sender<Event>,
    events: &Receiver<Event>,
) -> Result<Summary, Failure> {
    if start.protocol != PROTOCOL {
        return Err((
            "protocol",
            format!(
                "the host speaks protocol {}, this guest {PROTOCOL}",
                start.protocol
            ),
        ));
    }
    match start.task {
        Task::Probe => {
            host.send(&GuestMessage::Ready {
                boot_ms: since_start_ms(),
            });
            Ok(empty_summary(Some(0)))
        }
        Task::FormatScratch => Err(("format", "a container has no disk of its own to format".into())),
        Task::Job(job) => run_job(job, host, sender, events),
    }
}

/// Why a job cannot run in an engine's container; `None` when it can.
pub fn refused(job: &Job) -> Option<&'static str> {
    if job.docker {
        Some(
            "a Docker job needs a daemon of its own, which container mode does not give; it runs on a machine",
        )
    } else if job.trusted.is_some() {
        Some("a trusted job runs on a machine only")
    } else {
        None
    }
}

fn run_job(
    job: Job,
    host: &Host,
    sender: Sender<Event>,
    events: &Receiver<Event>,
) -> Result<Summary, Failure> {
    let container = |error: String| ("container", error);
    if let Some(why) = refused(&job) {
        return Err(container(why.into()));
    }
    let passwd = fs::read_to_string("/etc/passwd").unwrap_or_default();
    let (uid, _gid, home) =
        find_user(&passwd, USER).ok_or_else(|| container(format!("the image has no user {USER}")))?;
    // SAFETY: getuid takes nothing and cannot fail.
    let own = unsafe { libc::getuid() };
    if own == 0 || own != uid {
        return Err(container(format!(
            "the container runs as user {own}, not as {USER} ({uid}): container mode never runs a job as root"
        )));
    }
    let facts = JobFacts {
        host: job.host.clone(),
        limits: job.limits,
        disk_bytes: 0,
        docker: false,
    };
    share_in(
        SHARED,
        "job.json",
        &serde_json::to_string(&facts).unwrap_or_default(),
        0o644,
    )
    .map_err(container)?;
    let this =
        std::env::current_exe().map_err(|error| container(format!("the supervisor's own path: {error}")))?;
    for hook in ["job-started", "job-completed"] {
        share_in(
            SHARED,
            &format!("{hook}.sh"),
            &format!("#!/bin/sh\nexec {} hook {hook}\n", this.display()),
            0o755,
        )
        .map_err(container)?;
    }
    let argv = match &job.workload {
        Workload::ActionsRunner { jit_config } => vec![
            format!("{home}/run.sh"),
            "--jitconfig".to_string(),
            jit_config.expose().to_string(),
        ],
        Workload::Command { argv } => argv.clone(),
    };
    let env = workload_environment(
        &home,
        USER,
        &job.env
            .iter()
            .map(|(name, value)| (name.clone(), value.expose().clone()))
            .collect::<Vec<_>>(),
    );
    let workload = start(&argv, &env, &home).map_err(container)?;
    drop((argv, env));
    reap(workload, sender);
    host.send(&GuestMessage::Ready {
        boot_ms: since_start_ms(),
    });
    say(&format!(
        "workload started {} ms after the container",
        since_start_ms()
    ));

    // SAFETY: sysconf takes no pointers.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let mut watch = Watch {
        job: &job,
        host,
        sandbox: Sandbox {
            tree: Tree::supervisor(std::process::id()),
            cgroup: CGROUP,
            shared: SHARED,
            disk: None,
            kill: kill_all,
            gone: end_container,
        },
        started: Instant::now(),
        // The kernel's log is the host's; the control group's counter says what the kernel stopped.
        kernel_log: None,
        log: oom::Log::new(page),
        previous: Table::new([]),
        oom: None::<OomEvent>,
        count: oom::Count::default(),
        own_limit: oom::OwnLimit::default(),
        step: None,
        stop: None,
        killed: false,
        disk_peak: 0,
        heartbeat: Instant::now(),
    };
    let ended = watch.until_the_end(events);
    watch.last_look();
    let usage = usage_of(CGROUP, None);
    kill_all();
    Ok(Summary {
        end: watch
            .stop
            .map_or(GuestEnd::Exited, |(reason, _)| GuestEnd::Stopped(reason)),
        exit_code: ended.code(),
        signal: ended.signal(),
        disk_peak_bytes: 0,
        usage,
        oom: watch.oom,
        wall_ms: watch.started.elapsed().as_millis() as u64,
    })
}

/// Starts the workload in a session of its own, first in line when the kernel needs memory back, its output
/// through a pipe this process copies to the console.
fn start(argv: &[String], env: &[(String, String)], home: &str) -> Result<u32, String> {
    let (program, args) = argv.split_first().ok_or("the command is empty")?;
    let (output_read, output_write) = context("pipe", sys::pipe())?;
    let mut command = Command::new(program);
    command
        .args(args)
        .env_clear()
        .envs(env.iter().map(|(name, value)| (name, value)))
        .current_dir(home)
        .stdin(Stdio::null())
        .stdout(Stdio::from(context("pipe", output_write.try_clone())?))
        .stderr(Stdio::from(output_write));
    // SAFETY: setsid, open, write and close are async-signal-safe and read only what lives in this frame.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            // The supervisor and the workload share the container's limit: the kernel takes the workload's first.
            let file = libc::open(c"/proc/self/oom_score_adj".as_ptr(), libc::O_WRONLY);
            if file >= 0 {
                libc::write(file, c"1000".as_ptr().cast(), 4);
                libc::close(file);
            }
            Ok(())
        });
    }
    let child = context(&format!("start {program}"), command.spawn())?;
    drop(command);
    forward_output(output_read);
    // The child is reaped by `reap`, which takes every process's end as the container's first must.
    Ok(child.id())
}

/// Takes the end of every process that ends in the container, as its first process must, and says when the
/// workload's came.
fn reap(workload: u32, sender: Sender<Event>) {
    std::thread::spawn(move || {
        loop {
            let mut status = 0;
            // SAFETY: waitpid fills the integer we give it.
            let pid = unsafe { libc::waitpid(-1, &mut status, 0) };
            if pid < 0 {
                // No child right now: the workload is reaped once it is gone, so this is only between two.
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            if pid as u32 == workload {
                let _ = sender.send(Event::Ended(std::process::ExitStatus::from_raw(status)));
            }
        }
    });
}

//! Runs one job against a monitor: does what [`Machine`] says and tells it what happens.

use crate::control;
use crate::layout::Layout;
use crate::machine::{Action, Event, Limits, Machine};
use crate::request::{JobRequest, RequestError};
use crate::scratch::{self, GuestFiles, IMAGE_DISK, SCRATCH_DISK};
use crate::state::{JobState, Stage};
use crate::task::{describe, now_ns, put_down};
use cf_guest_proto::{
    GuestMessage, HostMessage, Job, PORT, PROTOCOL, Start, Stop, StopReason, Task, read_message,
    write_message,
};
use cf_vmm::{Channel, Disk, Network, Vm, VmSpec, Vmm};
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The guest this binary carries: the kernel and the initramfs with the supervisor that speaks its protocol.
#[derive(Debug, Clone, Copy)]
pub struct Guest<'a> {
    /// The kernel, an uncompressed arm64 `Image`.
    pub kernel: &'a [u8],
    /// The initramfs.
    pub initramfs: &'a [u8],
}

/// Why a job could not be run at all. A job that started and went wrong is no error: its state says how it ended.
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// The request is refused.
    #[error(transparent)]
    Request(#[from] RequestError),
    /// A job of this name has files in the state folder: it runs, or its end was not collected yet.
    #[error("a job named {0} is there already")]
    Exists(String),
    /// The image the job asks for is not installed.
    #[error("image {0} is not installed")]
    Image(String),
    /// A file or the socket in the state folder could not be made.
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    /// The job's disk could not be made.
    #[error("{0}")]
    Scratch(String),
}

/// How often the machine is asked what time it is when nothing else happens.
const TICK: Duration = Duration::from_millis(200);
/// A Unix socket's path may be this long on macOS, the shorter of the two systems.
const SOCKET_PATH_MAX: usize = 103;

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), RunError> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
        .map_err(|error| RunError::Io(path.display().to_string(), error))
}

/// Replaces a file whole: a reader sees the old state or the new one, never half of one.
fn replace(path: &Path, text: &str) {
    let partial = path.with_extension("json.new");
    if fs::write(&partial, text).is_ok() {
        let _ = fs::rename(&partial, path);
    }
}

enum Message {
    Machine(u32, Event),
    Channel(u32, Box<dyn Channel>),
    Asked(Event),
}

/// Runs the job to its end and returns its last state, which is also what `run/<job>.json` holds. `cancel`, once
/// set, ends the job as a stop with no grace would; the binary sets it from a signal.
///
/// The state file and the console log stay for the daemon to collect; the machine's own files are gone when this
/// returns, also when it returns an error.
pub fn run(
    vmm: &dyn Vmm,
    layout: &Layout,
    guest: Guest<'_>,
    request: JobRequest,
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<JobState, RunError> {
    request.check()?;
    let job = request.job.clone();
    let io = |path: &Path| {
        let path = path.display().to_string();
        move |error| RunError::Io(path, error)
    };
    let (state_path, socket_path, job_dir, console) = (
        layout.state(&job),
        layout.socket(&job),
        layout.job_dir(&job),
        layout.console(&job),
    );
    for folder in [
        layout.run(),
        console.parent().unwrap_or(Path::new("")).to_path_buf(),
    ] {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&folder)
            .map_err(io(&folder))?;
    }
    if socket_path.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(RunError::Io(
            socket_path.display().to_string(),
            std::io::Error::other("the state folder's path is too long for a socket"),
        ));
    }
    if state_path.exists() {
        return Err(RunError::Exists(job));
    }
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&job_dir)
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::AlreadyExists => RunError::Exists(job.clone()),
            _ => RunError::Io(job_dir.display().to_string(), error),
        })?;
    let result = run_in(vmm, layout, guest, request, limits, cancel);
    let _ = fs::remove_dir_all(&job_dir);
    let _ = fs::remove_file(&socket_path);
    result
}

fn run_in(
    vmm: &dyn Vmm,
    layout: &Layout,
    guest: Guest<'_>,
    mut request: JobRequest,
    limits: Limits,
    cancel: &AtomicBool,
) -> Result<JobState, RunError> {
    let job = request.job.clone();
    let (state_path, socket_path, job_dir, console) = (
        layout.state(&job),
        layout.socket(&job),
        layout.job_dir(&job),
        layout.console(&job),
    );
    let started = Instant::now();
    let files = GuestFiles {
        kernel: job_dir.join("kernel"),
        initramfs: job_dir.join("initramfs"),
        rootfs: layout.rootfs(&request.image),
    };
    if !files.rootfs.is_file() {
        return Err(RunError::Image(request.image));
    }
    write_private(&files.kernel, guest.kernel)?;
    write_private(&files.initramfs, guest.initramfs)?;

    // The registration leaves this process with the guest's task; nothing here keeps a second copy for longer
    // than a second boot may need it.
    let mut task = Some(Job {
        name: job.clone(),
        host: request.host.clone(),
        workload: std::mem::replace(
            &mut request.workload,
            cf_guest_proto::Workload::Command { argv: Vec::new() },
        ),
        limits: request.limits(),
        env: std::mem::take(&mut request.env),
        oom_grace_ms: 15_000,
    });

    let snapshot = |machine: Option<&Machine>, stage: Stage| JobState {
        job: job.clone(),
        pid: std::process::id(),
        started_at: now_ns() / 1_000_000_000 - started.elapsed().as_secs(),
        stage,
        vm_bytes: request.memory_bytes,
        limit_bytes: request.limits().memory_bytes,
        cpus: request.cpus,
        image: request.image.clone(),
        boots: machine.map_or(0, Machine::boots),
        boot_ms: machine.and_then(Machine::boot_ms),
        usage: machine.and_then(Machine::usage).cloned(),
        oom: machine.and_then(Machine::oom).cloned(),
        end: machine
            .filter(|_| stage == Stage::Ended)
            .and_then(|machine| machine.ended(now_ns() / 1_000_000_000)),
    };
    let published = Arc::new(Mutex::new(String::new()));
    let publish = |state: &JobState| {
        let text = serde_json::to_string(state).unwrap_or_default();
        replace(&state_path, &text);
        if let Ok(mut published) = published.lock() {
            *published = text;
        }
    };

    publish(&snapshot(None, Stage::Booting));

    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path)
        .map_err(|error| RunError::Io(socket_path.display().to_string(), error))?;
    let (sender, messages) = mpsc::channel::<Message>();
    let done = Arc::new(AtomicBool::new(false));
    let asked = sender.clone();
    control::serve(
        listener,
        published.clone(),
        move |event| drop(asked.send(Message::Asked(event))),
        done.clone(),
    );

    let scratch_disk = job_dir.join("scratch.img");
    let prepared = scratch::template(
        vmm,
        layout,
        &files,
        &request.image,
        request.scratch_bytes,
        &console,
    )
    .and_then(|template| scratch::copy(&template, &scratch_disk));
    if let Err(error) = prepared {
        done.store(true, Ordering::Relaxed);
        let _ = fs::remove_file(&state_path);
        return Err(RunError::Scratch(error));
    }
    let spec = VmSpec {
        kernel: files.kernel.clone(),
        initramfs: files.initramfs.clone(),
        memory_bytes: request.memory_bytes,
        cpus: request.cpus as usize,
        disks: vec![
            Disk {
                path: files.rootfs.clone(),
                id: IMAGE_DISK.into(),
                read_only: true,
            },
            Disk {
                path: scratch_disk,
                id: SCRATCH_DISK.into(),
                read_only: false,
            },
        ],
        network: Network::Nat,
        console,
    };
    // The job's clock starts here: the wait for a guest's `ready` is for the boot, not for a disk made before it.
    let (mut machine, first) = Machine::new(limits, started.elapsed());
    let generation = Arc::new(AtomicU32::new(0));
    let mut vm: Option<Arc<dyn Vm>> = None;
    let mut channel: Option<Box<dyn Channel>> = None;
    let mut actions: VecDeque<Action> = first.into();
    let mut events: VecDeque<Event> = VecDeque::new();
    let mut cancelled = false;
    loop {
        while let Some(action) = actions.pop_front() {
            match action {
                Action::Boot => {
                    let current = generation.fetch_add(1, Ordering::SeqCst) + 1;
                    if let Some(channel) = channel.take() {
                        channel.shutdown();
                    }
                    if let Some(old) = vm.take() {
                        put_down(old.as_ref(), Duration::from_secs(3));
                    }
                    match vmm.start(&spec) {
                        Ok(machine) => {
                            let machine: Arc<dyn Vm> = Arc::from(machine);
                            watch(machine.clone(), current, generation.clone(), sender.clone());
                            vm = Some(machine);
                        }
                        Err(error) => events.push_back(Event::StartFailed(error.to_string())),
                    }
                }
                Action::SendStart => {
                    let sent = match (&mut channel, &task) {
                        (Some(channel), Some(task)) => write_message(
                            channel,
                            &HostMessage::Start(Start {
                                protocol: PROTOCOL,
                                time_ns: now_ns(),
                                task: Task::Job(task.clone()),
                            }),
                        )
                        .is_ok(),
                        _ => false,
                    };
                    if !sent {
                        events.push_back(Event::ChannelClosed);
                    }
                }
                Action::SendStop { reason, grace } => {
                    if let Some(channel) = &mut channel {
                        let _ = write_message(
                            channel,
                            &HostMessage::Stop(Stop {
                                reason,
                                grace_ms: grace.as_millis() as u64,
                            }),
                        );
                    }
                }
                Action::CloseChannel => {
                    if let Some(channel) = &channel {
                        channel.shutdown();
                    }
                }
                Action::StopVm => {
                    if let Some(vm) = &vm
                        && vm.stop().is_err()
                    {
                        vm.kill();
                    }
                }
                Action::Publish => publish(&snapshot(Some(&machine), machine.stage())),
                Action::Finish => {
                    generation.fetch_add(1, Ordering::SeqCst);
                    done.store(true, Ordering::Relaxed);
                    if let Some(channel) = channel.take() {
                        channel.shutdown();
                    }
                    if let Some(vm) = vm.take() {
                        put_down(vm.as_ref(), Duration::from_secs(2));
                    }
                    let state = snapshot(Some(&machine), Stage::Ended);
                    publish(&state);
                    return Ok(state);
                }
            }
        }
        if machine.stage() != Stage::Booting {
            task = None;
        }
        let now = started.elapsed();
        if let Some(event) = events.pop_front() {
            actions.extend(machine.handle(event, now));
            continue;
        }
        if !cancelled && cancel.load(Ordering::Relaxed) {
            cancelled = true;
            actions.extend(machine.handle(
                Event::StopAsked {
                    reason: StopReason::Cancelled,
                    grace: Duration::ZERO,
                },
                now,
            ));
            continue;
        }
        let event = match messages.recv_timeout(TICK) {
            Ok(Message::Machine(of, event)) if of == generation.load(Ordering::SeqCst) => event,
            Ok(Message::Channel(of, opened)) if of == generation.load(Ordering::SeqCst) => {
                if let Ok(reader) = opened.try_clone() {
                    read(reader, of, sender.clone());
                }
                channel = Some(opened);
                Event::Connected
            }
            Ok(Message::Asked(event)) => event,
            // What a machine that was let go of still says.
            Ok(Message::Machine(..) | Message::Channel(..)) => continue,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => Event::Tick,
        };
        actions.extend(machine.handle(event, started.elapsed()));
    }
}

/// Watches one machine: opens the channel as soon as the guest listens, and says when the machine is gone.
fn watch(vm: Arc<dyn Vm>, of: u32, generation: Arc<AtomicU32>, sender: Sender<Message>) {
    std::thread::spawn(move || {
        let mut connected = false;
        while generation.load(Ordering::SeqCst) == of {
            if !connected && let Ok(channel) = vm.channel(PORT) {
                connected = true;
                if sender.send(Message::Channel(of, channel)).is_err() {
                    return;
                }
            }
            let nap = if connected {
                Duration::from_millis(250)
            } else {
                Duration::from_millis(25)
            };
            if let Some(exit) = vm.wait(nap) {
                let _ = sender.send(Message::Machine(of, Event::VmExited(describe(&exit))));
                return;
            }
        }
    });
}

/// Reads what the guest says until the channel ends.
fn read(mut channel: Box<dyn Channel>, of: u32, sender: Sender<Message>) {
    std::thread::spawn(move || {
        loop {
            let event = match read_message::<GuestMessage>(&mut channel) {
                Ok(Some(message)) => Event::Guest(message),
                _ => Event::ChannelClosed,
            };
            let last = event == Event::ChannelClosed;
            if sender.send(Message::Machine(of, event)).is_err() || last {
                return;
            }
        }
    });
}

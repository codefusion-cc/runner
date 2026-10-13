//! [`ChVmm`] and its machines.

use crate::cgroup::{Delegation, Limits, MachineGroup, machine_name, owner};
use crate::command::{self, Files};
use crate::cpus::{self, List};
use crate::ending::{self, Ending, Recent, Status};
use crate::events::{Event, Events};
use crate::sandbox::{self, Access, Sandbox};
use crate::vsock;
use cf_nft::Ip;
use cf_vmm::{Channel, ExitCell, Network, Vm, VmExit, VmSpec, Vmm, VmmError};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// How long a machine may take from its start to running.
const BOOT_WAIT: Duration = Duration::from_secs(20);
/// How long ending a machine may take before its files are left to the next host that starts.
const CLEANUP_WAIT: Duration = Duration::from_secs(5);
/// The longest path a Unix socket can have, less room for the monitor's own suffixes.
const SOCKET_PATH_MAX: usize = 100;

/// Where [`ChVmm`] finds its programs and keeps its machines.
#[derive(Debug, Clone)]
pub struct Config {
    /// A private folder where each machine has a folder of its own while it runs.
    pub dir: PathBuf,
    /// The cloud-hypervisor program.
    pub cloud_hypervisor: PathBuf,
    /// The pasta program.
    pub pasta: PathBuf,
    /// The program whose `vm-sandbox` subcommand is [`crate::sandbox`]: the one that hosts this crate.
    pub sandbox: PathBuf,
    /// The host CPUs machines run on; `None` for the fastest this process may use.
    pub host_cpus: Option<Vec<u32>>,
    /// The networks of the host's interfaces, which no guest reaches.
    pub host_networks: Vec<(Ip, u8)>,
}

/// The monitor: cloud-hypervisor under KVM.
pub struct ChVmm {
    config: Config,
    delegation: Delegation,
    cores: Vec<u32>,
    next: AtomicU64,
}

impl ChVmm {
    /// Makes the monitor of this process: KVM must be there, and the cgroup this process runs in delegated to it
    /// ([`Delegation::claim`]). What hosts that are gone left behind is removed.
    pub fn new(mut config: Config) -> Result<Self, VmmError> {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map_err(|e| VmmError::Unsupported(format!("/dev/kvm: {e}")))?;
        let delegation = Delegation::claim().map_err(VmmError::Unsupported)?;
        let allowed = cpus::host::allowed(delegation.machines()).map_err(VmmError::Unsupported)?;
        let cores = cpus::host_cores(
            config.host_cpus.as_deref(),
            &allowed,
            cf_host_linux::fast_cpus(Path::new("/")).as_deref(),
        )
        .map_err(VmmError::Unsupported)?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&config.dir)
            .map_err(|e| VmmError::Unsupported(format!("{}: {e}", config.dir.display())))?;
        // The sandbox shows the monitor each file at its own path, which must be the one path to it.
        for path in [&mut config.dir, &mut config.cloud_hypervisor] {
            *path = fs::canonicalize(&path)
                .map_err(|e| VmmError::Unsupported(format!("{}: {e}", path.display())))?;
        }
        delegation.sweep(alive);
        if let Ok(entries) = fs::read_dir(&config.dir) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .and_then(owner)
                    .is_some_and(|host| !alive(host))
                {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        Ok(ChVmm {
            config,
            delegation,
            cores,
            next: AtomicU64::new(0),
        })
    }

    /// The host CPUs machines run on.
    pub fn cores(&self) -> &[u32] {
        &self.cores
    }

    /// Whether machines are held to their cores by their cgroup as well as by their vCPUs' affinity.
    pub fn has_cpuset(&self) -> bool {
        self.delegation.has_cpuset()
    }
}

/// `spec` with each of its files named by its one path, without links, `.` or `..`: the sandbox shows the monitor
/// each at that path.
fn resolved(spec: &VmSpec) -> Result<VmSpec, VmmError> {
    let resolve =
        |path: &Path| fs::canonicalize(path).map_err(|e| VmmError::Spec(format!("{}: {e}", path.display())));
    let mut resolved = spec.clone();
    resolved.kernel = resolve(&spec.kernel)?;
    resolved.initramfs = resolve(&spec.initramfs)?;
    resolved.console = resolve(&spec.console)?;
    for disk in &mut resolved.disks {
        disk.path = resolve(&disk.path)?;
    }
    Ok(resolved)
}

/// Whether a process of this id is running.
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal 0 takes no pointers and sends nothing.
    pid > 0
        && (unsafe { libc::kill(pid, 0) } == 0
            || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

impl Vmm for ChVmm {
    fn start(&self, spec: &VmSpec) -> Result<Box<dyn Vm>, VmmError> {
        spec.validate()?;
        // The monitor is shown its console, which must be there to be shown.
        fs::OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&spec.console)
            .map_err(|e| VmmError::Start(format!("{}: {e}", spec.console.display())))?;
        let spec = &resolved(spec)?;
        let name = machine_name(std::process::id(), self.next.fetch_add(1, Ordering::Relaxed));
        let dir = self.config.dir.join(&name);
        let (vsock, events) = (dir.join("vsock"), dir.join("events"));
        if vsock.as_os_str().len() > SOCKET_PATH_MAX {
            return Err(VmmError::Start(format!(
                "{} is too long a path for a socket",
                vsock.display()
            )));
        }
        let monitor = command::monitor(
            &self.config.cloud_hypervisor,
            spec,
            Files {
                vsock: &vsock,
                events: &events,
            },
            &self.cores,
        )
        .map_err(VmmError::Spec)?;
        let mut sandbox = Sandbox::for_machine(
            spec,
            &self.config.cloud_hypervisor,
            &dir,
            self.config.host_networks.clone(),
        );
        for fact in sandbox::HOST_FACTS {
            if Path::new(fact).exists() {
                sandbox.show(fact, Access::Read);
            }
        }
        let sandbox = sandbox.command(&self.config.sandbox, monitor);
        let command = match spec.network {
            Network::Nat => command::pasta(&self.config.pasta, sandbox),
            Network::None => sandbox,
        };
        let memory_max = spec
            .memory_bytes
            .saturating_add(crate::overhead(spec.memory_bytes));
        let cpuset = self
            .delegation
            .has_cpuset()
            .then(|| List(&self.cores).to_string());
        let limits = Limits::of(spec.memory_bytes, crate::overhead(spec.memory_bytes), cpuset)
            .map_err(VmmError::Spec)?;

        let _ = fs::remove_dir_all(&dir);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| VmmError::Start(format!("{}: {e}", dir.display())))?;
        let group = match self.delegation.machine(&name, &limits) {
            Ok(group) => group,
            Err(error) => {
                let _ = fs::remove_dir_all(&dir);
                return Err(VmmError::Start(error));
            }
        };
        let shared = Arc::new(Shared {
            exit: ExitCell::default(),
            stop_asked: AtomicBool::new(false),
            group,
            dir,
            vsock,
            memory_max,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        });
        let machine = ChVm {
            shared: Arc::clone(&shared),
        };
        let pipes = match Pipes::open(&events) {
            Ok(pipes) => pipes,
            Err(error) => {
                shared.finish();
                return Err(VmmError::Start(error));
            }
        };
        let watcher = Arc::clone(&shared);
        if let Err(error) = std::thread::Builder::new()
            .name(name)
            .spawn(move || watcher.run(command, pipes))
        {
            shared.finish();
            return Err(VmmError::Start(format!("no thread for the machine: {error}")));
        }
        let deadline = Instant::now() + BOOT_WAIT;
        let mut state = shared.state();
        loop {
            if state.booted && !state.finished {
                return Ok(Box::new(machine));
            }
            if state.finished {
                let why = match shared.exit.wait(Duration::ZERO) {
                    Some(VmExit::Error(why)) => why,
                    other => format!("it ended at once ({other:?})"),
                };
                drop(state);
                return Err(VmmError::Start(why));
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                let said = ending::tail(&state.output.text(), 8);
                drop(state);
                drop(machine);
                return Err(VmmError::Start(format!(
                    "it did not run within {BOOT_WAIT:?}: {said}"
                )));
            };
            state = shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// What a machine's threads and its handle share.
struct Shared {
    exit: ExitCell,
    stop_asked: AtomicBool,
    group: MachineGroup,
    dir: PathBuf,
    vsock: PathBuf,
    memory_max: u64,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// The monitor said the machine runs.
    booted: bool,
    /// The guest reset its machine.
    reset: bool,
    /// The end of what the monitor, pasta and the sandbox wrote.
    output: Recent,
    /// The machine ended and its cgroup and files are gone, as far as they could be removed.
    finished: bool,
}

/// The two streams a machine's processes write to: the monitor's events, through a FIFO that the host holds a
/// writer of until the processes are gone (so the reader never sees an end before the monitor opened it), and what
/// they write to standard output and error.
struct Pipes {
    events: File,
    events_held: File,
    output: OwnedFd,
    output_end: OwnedFd,
}

impl Pipes {
    fn open(events: &Path) -> Result<Self, String> {
        let path =
            std::ffi::CString::new(events.as_os_str().as_encoded_bytes()).map_err(|e| e.to_string())?;
        // SAFETY: the path is NUL terminated and lives through the call.
        if unsafe { libc::mkfifo(path.as_ptr(), 0o600) } != 0 {
            return Err(format!("{}: {}", events.display(), io::Error::last_os_error()));
        }
        let say = |e: io::Error| format!("{}: {e}", events.display());
        let reader = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(events)
            .map_err(say)?;
        let held = fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(events)
            .map_err(say)?;
        let mut ends = [0; 2];
        // SAFETY: pipe2 writes two descriptors into the array, which lives through the call.
        if unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(format!("pipe: {}", io::Error::last_os_error()));
        }
        // SAFETY: both descriptors were just made and belong to nobody else.
        let (output, output_end) = unsafe {
            use std::os::fd::FromRawFd as _;
            (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1]))
        };
        Ok(Pipes {
            events: reader,
            events_held: held,
            output,
            output_end,
        })
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn ended(&self) -> bool {
        self.exit.wait(Duration::ZERO).is_some()
    }

    /// Ends every process of the machine at once.
    fn plug(&self) {
        self.group.kill();
    }

    /// Starts the machine's first process, waits for it, says how the machine ended, and removes what it left.
    fn run(self: Arc<Self>, command: Vec<OsString>, pipes: Pipes) {
        let Pipes {
            events,
            events_held,
            output,
            output_end,
        } = pipes;
        let spawned = self.spawn(&command, output_end);
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                self.exit.end(VmExit::Error(error));
                self.finish();
                return;
            }
        };
        let (closed, output_done) = mpsc::channel();
        let reader = Arc::clone(&self);
        let output_thread = std::thread::Builder::new().spawn(move || {
            reader.read(events, File::from(output));
            let _ = closed.send(());
        });
        let status = match child.wait() {
            Ok(status) => match status.code() {
                Some(code) => Status::Code(code),
                None => Status::Signal(status.signal().unwrap_or(0)),
            },
            Err(error) => {
                self.exit
                    .end(VmExit::Error(format!("waiting for the monitor: {error}")));
                Status::Signal(0)
            }
        };
        // What the first process leaves behind ends with it, and only then do the streams end.
        self.plug();
        let began = Instant::now();
        while self.group.populated() && began.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(events_held);
        if output_thread.is_ok() {
            let _ = output_done.recv_timeout(Duration::from_secs(2));
        }
        let state = self.state();
        let text = state.output.text();
        let exit = ending::exit_of(&Ending {
            stop_asked: self.stop_asked.load(Ordering::SeqCst),
            status,
            oom_kills: self.group.oom_kills(),
            reset: state.reset,
            memory_max: self.memory_max,
            output: &text,
        });
        drop(state);
        self.exit.end(exit);
        self.finish();
    }

    fn spawn(&self, command: &[OsString], output: OwnedFd) -> Result<std::process::Child, String> {
        let (program, args) = command.split_first().ok_or("no command")?;
        let procs = self.group.procs()?;
        let errors = output.try_clone().map_err(|e| e.to_string())?;
        let mut process = Command::new(program);
        process
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(errors))
            // Signals meant for the host's process group (a ^C at a terminal) do not reach the machine: the host
            // ends it.
            .process_group(0);
        let procs_fd = procs.as_raw_fd();
        // SAFETY: between fork and exec the child makes only system calls on a descriptor opened before the fork.
        unsafe {
            process.pre_exec(move || {
                if libc::write(procs_fd, b"0".as_ptr().cast(), 1) != 1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        process
            .spawn()
            .map_err(|e| format!("{}: {e}", program.to_string_lossy()))
    }

    /// Reads the events and the output until both end. A reset ends the machine at once.
    fn read(&self, events: File, output: File) {
        let mut parser = Events::default();
        let mut open = [Some(events), Some(output)];
        let mut buffer = vec![0u8; 16 << 10];
        while open.iter().any(Option::is_some) {
            let mut polled: Vec<libc::pollfd> = open
                .iter()
                .flatten()
                .map(|file| libc::pollfd {
                    fd: file.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                })
                .collect();
            // SAFETY: poll reads and writes the array, of the length given, which lives through the call.
            if unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, -1) } < 0 {
                if io::Error::last_os_error().kind() == ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            let ready: Vec<i32> = polled
                .iter()
                .filter(|fd| fd.revents != 0)
                .map(|fd| fd.fd)
                .collect();
            for (index, slot) in open.iter_mut().enumerate() {
                let Some(file) = slot.as_mut().filter(|file| ready.contains(&file.as_raw_fd())) else {
                    continue;
                };
                match file.read(&mut buffer) {
                    Ok(0) => *slot = None,
                    Ok(read) if index == 0 => {
                        for event in parser.push(&buffer[..read]) {
                            self.heard(event);
                        }
                    }
                    Ok(read) => {
                        self.state().output.push(&buffer[..read]);
                    }
                    Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) => {}
                    Err(_) => *slot = None,
                }
            }
        }
    }

    fn heard(&self, event: Event) {
        let mut state = self.state();
        match event {
            Event::Booted => state.booted = true,
            Event::Rebooting => {
                state.reset = true;
                drop(state);
                self.plug();
                return;
            }
            Event::Shutdown | Event::Other => return,
        }
        drop(state);
        self.changed.notify_all();
    }

    /// Removes the machine's cgroup and folder, ending what is left in them, and says it is done.
    fn finish(&self) {
        self.plug();
        self.group.remove(Duration::from_secs(2));
        let _ = fs::remove_dir_all(&self.dir);
        self.state().finished = true;
        self.changed.notify_all();
    }
}

/// A machine of [`ChVmm`].
struct ChVm {
    shared: Arc<Shared>,
}

impl Vm for ChVm {
    fn channel(&self, port: u32) -> Result<Box<dyn Channel>, VmmError> {
        let refuse = |reason: String| VmmError::Connect { port, reason };
        if self.shared.ended() {
            return Err(refuse("the machine has ended".into()));
        }
        let stream = UnixStream::connect(&self.shared.vsock).map_err(|e| refuse(e.to_string()))?;
        vsock::handshake(&stream, port, vsock::HANDSHAKE).map_err(refuse)?;
        Ok(Box::new(Stream(stream)))
    }

    fn wait(&self, timeout: Duration) -> Option<VmExit> {
        self.shared.exit.wait(timeout)
    }

    fn stop(&self) -> Result<(), VmmError> {
        if let Some(exit) = self.shared.exit.wait(Duration::ZERO) {
            return Err(VmmError::Stop(format!("the machine has ended: {exit:?}")));
        }
        self.shared.stop_asked.store(true, Ordering::SeqCst);
        self.shared.plug();
        Ok(())
    }

    fn kill(&self) {
        self.shared.stop_asked.store(true, Ordering::SeqCst);
        self.shared.plug();
        self.shared.exit.end(VmExit::Stopped);
    }

    fn resident(&self) -> Option<u64> {
        if self.shared.ended() {
            return None;
        }
        self.shared.group.resident()
    }
}

impl Drop for ChVm {
    fn drop(&mut self) {
        self.shared.stop_asked.store(true, Ordering::SeqCst);
        self.shared.plug();
        let deadline = Instant::now() + CLEANUP_WAIT;
        let mut state = self.shared.state();
        while !state.finished {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                return;
            };
            state = self
                .shared
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// A connection to a guest's port.
struct Stream(UnixStream);

impl Read for Stream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl Write for Stream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Channel for Stream {
    fn try_clone(&self) -> io::Result<Box<dyn Channel>> {
        Ok(Box::new(Stream(self.0.try_clone()?)))
    }

    fn shutdown(&self) {
        let _ = self.0.shutdown(Shutdown::Both);
    }
}

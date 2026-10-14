//! The machine's first process: brings the guest up, answers the host, runs the one task it is given, powers off.

use super::sys::{self, say};
use super::{docker, job, net};
use cf_guest_proto::{
    GuestMessage, HostMessage, PORT, PROTOCOL, Start, Summary, Task, read_message, write_message,
};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MODULE_LIST: &str = "/etc/modules";
const MODULE_DIR: &str = "/lib/modules";
const CONSOLE: &str = "/dev/hvc0";
/// A machine no host talks to powers itself off after this long.
const HOST_WAIT: Duration = Duration::from_secs(120);
/// After the last message, how long the host has to take it before the power goes.
const LAST_WORDS: Duration = Duration::from_secs(3);
/// The host is this address on the machine's socket.
const HOST_CID: u32 = 2;
/// The supervisor's own copy, which the job's container and its hooks run.
pub const SELF_COPY: &str = "/run/cf/supervisor";

/// The connection to the host, shared by the threads that send.
#[derive(Clone)]
pub struct Host(Arc<Mutex<File>>);

impl Host {
    /// Sends a message; a host that is gone is found by the reader, not here.
    pub fn send(&self, message: &GuestMessage) {
        if let Ok(mut stream) = self.0.lock() {
            let _ = write_message(&mut *stream, message);
            let _ = stream.flush();
        }
    }
}

/// What reaches the supervisor from outside.
pub enum Event {
    /// A message of the host's.
    Host(HostMessage),
    /// The host closed the connection or sent what is no message.
    HostGone,
    /// The job's launcher ended, with this wait status.
    Ended(std::process::ExitStatus),
}

pub fn run() -> ! {
    std::panic::set_hook(Box::new(|panic| {
        say(&format!("stopped by a mistake in the supervisor: {panic}"));
        sys::power_off();
    }));
    for (source, target, kind) in [
        ("proc", "/proc", "proc"),
        ("sysfs", "/sys", "sysfs"),
        ("devtmpfs", "/dev", "devtmpfs"),
    ] {
        let _ = sys::mount_at(source, target, kind, 0, "");
    }
    let console = wait_for_console();
    sys::take_console(&console);
    drop(console);
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
        .unwrap_or_default()
        .trim()
        .to_string();
    say(&format!("kernel {kernel}"));

    if let Err(error) = load_modules(MODULE_LIST, MODULE_DIR).and_then(|()| own_root()) {
        say(&error);
        sys::power_off();
    }

    let (stream, listener) = match accept_host() {
        Ok(accepted) => accepted,
        Err(error) => {
            say(&format!("no host: {error}"));
            sys::power_off();
        }
    };
    let host = Host(Arc::new(Mutex::new(
        stream.try_clone().unwrap_or_else(|_| sys::power_off()),
    )));
    host.send(&GuestMessage::Hello {
        protocol: PROTOCOL,
        kernel,
    });

    let mut reader = stream;
    let start = match read_message::<HostMessage>(&mut reader) {
        Ok(Some(HostMessage::Start(start))) => start,
        other => {
            say(&format!("the host did not start a task: {}", describe(&other)));
            sys::power_off();
        }
    };
    let (sender, events) = mpsc::channel();
    let from_host = sender.clone();
    std::thread::spawn(move || {
        loop {
            match read_message::<HostMessage>(&mut reader) {
                Ok(Some(message)) => {
                    if from_host.send(Event::Host(message)).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = from_host.send(Event::HostGone);
                    return;
                }
            }
        }
    });

    let result = perform(start, &host, sender, &events);
    match result {
        Ok(summary) => host.send(&GuestMessage::Exited(summary)),
        Err((stage, message)) => {
            say(&format!("{stage}: {message}"));
            host.send(&GuestMessage::Failed {
                stage: stage.to_string(),
                message,
            });
        }
    }
    // The host closes the connection once it has the last message; a host that does not gets a moment, no more.
    let deadline = std::time::Instant::now() + LAST_WORDS;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match events.recv_timeout(left) {
            Ok(Event::HostGone) | Err(_) => break,
            Ok(_) => {}
        }
    }
    drop(listener);
    say("power off");
    sys::power_off()
}

/// A failed task: the stage it failed in and why.
pub type Failure = (&'static str, String);

fn perform(
    start: Start,
    host: &Host,
    sender: mpsc::Sender<Event>,
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
    sys::set_clock(start.time_ns);
    match start.task {
        Task::Probe => {
            host.send(&GuestMessage::Ready {
                boot_ms: sys::uptime_ms(),
            });
            Ok(job::empty_summary(Some(0)))
        }
        Task::FormatScratch => {
            host.send(&GuestMessage::Ready {
                boot_ms: sys::uptime_ms(),
            });
            job::format_scratch().map_err(|error| ("format", error))
        }
        Task::Job(task) => {
            let told = task.blocked.clone();
            let network = std::thread::spawn(move || net::up(told));
            job::run(task, network, host, sender, events)
        }
    }
}

fn describe(read: &Result<Option<HostMessage>, cf_guest_proto::ProtoError>) -> String {
    match read {
        Ok(Some(_)) => "another message came first".into(),
        Ok(None) => "it closed the connection".into(),
        Err(error) => error.to_string(),
    }
}

/// The virtio console appears a moment after devtmpfs is mounted.
fn wait_for_console() -> File {
    for _ in 0..200 {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(CONSOLE) {
            return file;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    sys::power_off()
}

/// Loads the modules a list names, in its order, from a folder. A module the kernel has already is no error.
pub fn load_modules(list: &str, dir: &str) -> Result<(), String> {
    let list = fs::read_to_string(list).unwrap_or_default();
    for name in list.lines().filter(|line| !line.is_empty()) {
        let path = format!("{dir}/{name}.ko");
        let file = File::open(&path).map_err(|error| format!("module {path}: {error}"))?;
        let parameters = sys::cstr("");
        // SAFETY: finit_module takes a descriptor we own for the call and a NUL terminated parameter string.
        let result =
            unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), parameters.as_ptr(), 0) };
        let error = io::Error::last_os_error();
        if result != 0 && error.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("module {name}: {error}"));
        }
    }
    Ok(())
}

/// Moves the supervisor from the boot archive's root, which a container cannot leave behind, to a memory file
/// system of its own, and frees what the archive held. The modules only a Docker job loads come along, until the
/// job says whether it is one.
fn own_root() -> Result<(), String> {
    const NEW: &str = "/newroot";
    sys::mount_at("root", NEW, "tmpfs", 0, "mode=0755")?;
    for directory in [
        "proc",
        "sys",
        "dev",
        "run/cf",
        "mnt/image",
        "mnt/scratch",
        "mnt/root",
        "etc",
        &docker::MODULE_DIR[1..],
    ] {
        sys::context("make the root", fs::create_dir_all(format!("{NEW}/{directory}")))?;
    }
    let docker_modules = fs::read_to_string(docker::MODULE_LIST).unwrap_or_default();
    for name in docker_modules.lines().filter(|line| !line.is_empty()) {
        sys::context(
            "keep a module",
            fs::copy(
                format!("{MODULE_DIR}/{name}.ko"),
                format!("{NEW}{}/{name}.ko", docker::MODULE_DIR),
            ),
        )?;
    }
    sys::context(
        "keep the module list",
        fs::write(format!("{NEW}{}", docker::MODULE_LIST), docker_modules),
    )?;
    sys::context(
        "copy the supervisor",
        fs::copy("/init", format!("{NEW}{SELF_COPY}")),
    )?;
    let _ = fs::remove_dir_all(MODULE_DIR);
    let _ = fs::remove_file("/init");
    for directory in ["proc", "sys", "dev"] {
        sys::context(
            "move a mount",
            sys::mount(
                &format!("/{directory}"),
                &format!("{NEW}/{directory}"),
                "",
                libc::MS_MOVE,
                "",
            ),
        )?;
    }
    sys::context("enter the root", std::env::set_current_dir(NEW))?;
    sys::context("move the root", sys::mount(".", "/", "", libc::MS_MOVE, ""))?;
    let here = sys::cstr(".");
    // SAFETY: chroot takes one NUL terminated path.
    sys::context(
        "change the root",
        sys::check(unsafe { libc::chroot(here.as_ptr()) }).map(drop),
    )?;
    sys::context("enter the root", std::env::set_current_dir("/"))?;
    sys::mount_at("cgroup2", "/sys/fs/cgroup", "cgroup2", 0, "")
}

/// Waits for the host's one connection. The listening socket comes back too: held open, it keeps the port from a
/// job that would listen there in the supervisor's place.
fn accept_host() -> Result<(File, OwnedFd), String> {
    // SAFETY: socket takes no pointers.
    let raw = sys::context(
        "socket",
        sys::check(unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) }),
    )?;
    // SAFETY: the descriptor was just made and belongs to nobody else.
    let listener = unsafe { OwnedFd::from_raw_fd(raw) };
    // SAFETY: an all-zero sockaddr_vm is a valid value to fill in.
    let mut address: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
    address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
    address.svm_cid = libc::VMADDR_CID_ANY;
    address.svm_port = PORT;
    let length = std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
    // SAFETY: bind reads `length` bytes of the address we own.
    sys::context(
        "bind",
        sys::check(unsafe { libc::bind(listener.as_raw_fd(), (&raw const address).cast(), length) }),
    )?;
    // SAFETY: listen takes no pointers.
    sys::context(
        "listen",
        sys::check(unsafe { libc::listen(listener.as_raw_fd(), 1) }),
    )?;
    let deadline = std::time::Instant::now() + HOST_WAIT;
    loop {
        let left = deadline
            .checked_duration_since(std::time::Instant::now())
            .ok_or("nobody connected")?;
        let mut poll = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd and a length of 1.
        unsafe { libc::poll(&mut poll, 1, left.as_millis() as libc::c_int) };
        if poll.revents & libc::POLLIN == 0 {
            continue;
        }
        // SAFETY: an all-zero sockaddr_vm is a valid value for accept to fill.
        let mut peer: libc::sockaddr_vm = unsafe { std::mem::zeroed() };
        let mut peer_length = length;
        // SAFETY: accept4 writes at most `peer_length` bytes into the address we own.
        let accepted = unsafe {
            libc::accept4(
                listener.as_raw_fd(),
                (&raw mut peer).cast(),
                &mut peer_length,
                libc::SOCK_CLOEXEC,
            )
        };
        if accepted < 0 {
            continue;
        }
        // SAFETY: the descriptor was just made and belongs to nobody else.
        let stream = unsafe { File::from_raw_fd(accepted) };
        if peer.svm_cid == HOST_CID {
            return Ok((stream, listener));
        }
    }
}

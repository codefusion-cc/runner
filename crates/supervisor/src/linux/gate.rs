//! The gate's hands (see `cf_supervisor::gate`): it runs in the job's container as `Runner.Worker spawnclient <in>
//! <out>`, the way the Actions runner's listener starts its Worker, as the job's user, before anything of the job.
//!
//! Refused, it prints why and ends with 1, before the Worker starts. Accepted, it becomes the Worker in the same
//! process, so the listener's waits, signals and exit codes are the Worker's own: a child of its own hands the
//! Worker the message the gate read and then everything else the listener sends, through a pipe in place of `<in>`.

use cf_supervisor::gate::{self, CONFIG, GateConfig};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::sys;

/// The gate's file as the container sees the shared folder.
const SHARED_CONFIG: &str = "/run/codefusion";
/// The longest gate file read: its rules are a few hundred bytes.
const CONFIG_MAX: u64 = 64 << 10;

/// Runs the gate with the listener's arguments. Never returns: it ends, or it is the Worker.
pub fn run(args: &[&str]) -> ! {
    let why = match gate(args) {
        Ok(never) => match never {},
        Err(why) => why,
    };
    eprintln!("{}", gate::refusal(&why));
    std::process::exit(1)
}

enum Never {}

fn gate(args: &[&str]) -> Result<Never, String> {
    let ["spawnclient", input, output] = args else {
        return Err(format!(
            "the Worker was started with {args:?}, not as the listener starts it"
        ));
    };
    let input = descriptor(input)?;
    let output = descriptor(output)?;
    let config = config()?;
    // SAFETY: the listener handed this descriptor to the Worker for the Worker alone; nothing else here owns it.
    let mut channel = File::from(unsafe { OwnedFd::from_raw_fd(input) });
    let verdict = gate::read_request(&mut channel).and_then(|request| {
        let verdict = gate::decide(&config.policy, &request);
        leave(&config.verdict, &gate::verdict_line(&verdict));
        verdict.map(|_| request)
    });
    let request = verdict?;
    hand_on(&config.worker, request.frame, channel, (input, output))
}

/// A descriptor the listener passed: a number of an open one, and not the standard three.
fn descriptor(text: &str) -> Result<RawFd, String> {
    let fd = text
        .parse::<RawFd>()
        .ok()
        .filter(|fd| *fd > 2)
        .ok_or_else(|| format!("the Worker's channel {text:?} is not a descriptor"))?;
    // SAFETY: fcntl F_GETFD takes no pointers; it only asks whether the descriptor is open.
    sys::check(unsafe { libc::fcntl(fd, libc::F_GETFD) })
        .map_err(|error| format!("the Worker's channel {fd} is not open ({error})"))?;
    Ok(fd)
}

fn config() -> Result<GateConfig, String> {
    let path = format!("{SHARED_CONFIG}/{CONFIG}");
    let mut text = String::new();
    File::open(&path)
        .and_then(|file| file.take(CONFIG_MAX).read_to_string(&mut text))
        .map_err(|error| format!("the gate has no rules ({path}: {error})"))?;
    serde_json::from_str(&text).map_err(|error| format!("the gate's rules do not read ({error})"))
}

/// Leaves the verdict where the supervisor reads it after the job; the job goes on without it.
fn leave(path: &str, line: &str) {
    let _ = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()));
}

/// Becomes the Worker: a pipe takes the place of the listener's channel at the same number, and a child first
/// writes the message the gate read into it, then copies whatever else the listener sends.
fn hand_on(
    worker: &str,
    frame: Vec<u8>,
    channel: File,
    (input, output): (RawFd, RawFd),
) -> Result<Never, String> {
    let (read, write) = sys::context("pipe", sys::pipe())?;
    // SAFETY: getpid takes no pointers.
    let gate = unsafe { libc::getpid() };
    if sys::fork()? == 0 {
        drop(read);
        proxy(gate, frame, channel, write, output);
    }
    drop(write);
    drop(channel);
    // SAFETY: dup2 on descriptors this process owns; the copy at `input` is open across exec, `read` is not.
    sys::check(unsafe { libc::dup2(read.as_raw_fd(), input) })
        .map_err(|error| format!("the Worker's channel could not be handed on ({error})"))?;
    drop(read);
    let error = Command::new(worker)
        .args(["spawnclient", &input.to_string(), &output.to_string()])
        .exec();
    Err(format!("the Worker {worker} did not start ({error})"))
}

/// The child that feeds the Worker. It dies with the Worker, and holds nothing the listener waits on to close.
fn proxy(gate: libc::pid_t, frame: Vec<u8>, mut channel: File, write: OwnedFd, output: RawFd) -> ! {
    // SAFETY: prctl and getppid take no pointers. The parent may have gone before the signal was asked for.
    unsafe {
        libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
        if libc::getppid() != gate {
            libc::_exit(0);
        }
        libc::close(output);
    }
    if let Ok(null) = OpenOptions::new().read(true).write(true).open("/dev/null") {
        for fd in 0..=2 {
            // SAFETY: dup2 onto the standard descriptors of this process.
            unsafe { libc::dup2(null.as_raw_fd(), fd) };
        }
    }
    let mut pipe = File::from(write);
    let _ = pipe
        .write_all(&frame)
        .and_then(|()| io::copy(&mut channel, &mut pipe));
    // SAFETY: _exit takes no pointers; nothing of this copy of the gate needs tidying.
    unsafe { libc::_exit(0) }
}

//! Small wrappers around the system calls the supervisor makes.

use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

/// A C string of a text without a NUL byte; one with a NUL becomes empty, which every call here refuses.
pub fn cstr(text: &str) -> CString {
    CString::new(text).unwrap_or_default()
}

/// Turns a system call's `-1` into the error behind it.
pub fn check(result: libc::c_int) -> io::Result<libc::c_int> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// An error that says what was being done.
pub fn context<T>(what: &str, result: io::Result<T>) -> Result<T, String> {
    result.map_err(|error| format!("{what}: {error}"))
}

/// `mount(2)`.
pub fn mount(source: &str, target: &str, kind: &str, flags: libc::c_ulong, data: &str) -> io::Result<()> {
    let (source, target, kind, data) = (cstr(source), cstr(target), cstr(kind), cstr(data));
    let kind_pointer = if kind.as_bytes().is_empty() {
        std::ptr::null()
    } else {
        kind.as_ptr()
    };
    let data_pointer = if data.as_bytes().is_empty() {
        std::ptr::null()
    } else {
        data.as_ptr().cast()
    };
    // SAFETY: every pointer is a NUL terminated string that lives through the call, or null where mount allows it.
    check(unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind_pointer,
            flags,
            data_pointer,
        )
    })
    .map(drop)
}

/// A mount that must work, with its target directory made first.
pub fn mount_at(
    source: &str,
    target: &str,
    kind: &str,
    flags: libc::c_ulong,
    data: &str,
) -> Result<(), String> {
    let _ = fs::create_dir_all(target);
    context(
        &format!("mount {kind} at {target}"),
        mount(source, target, kind, flags, data),
    )
}

/// Writes a file of the kernel's (a sysctl, a cgroup setting).
pub fn set(path: impl AsRef<Path>, value: &str) -> Result<(), String> {
    let path = path.as_ref();
    context(&format!("write {}", path.display()), fs::write(path, value))
}

/// A number from a file of the kernel's; 0 when it is not one (`max`) or not there.
pub fn number(path: impl AsRef<Path>) -> u64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// A pipe, both ends closed on exec.
pub fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut ends = [0; 2];
    // SAFETY: pipe2 fills the two descriptors of an array we own.
    check(unsafe { libc::pipe2(ends.as_mut_ptr(), libc::O_CLOEXEC) })?;
    // SAFETY: both descriptors were just made and belong to nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) })
}

/// Milliseconds since the kernel started.
pub fn uptime_ms() -> u64 {
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime fills the timespec we give it.
    unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) };
    time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000
}

/// Sets the wall clock, once, from the host's.
pub fn set_clock(time_ns: u64) {
    let time = libc::timespec {
        tv_sec: (time_ns / 1_000_000_000) as _,
        tv_nsec: (time_ns % 1_000_000_000) as libc::c_long,
    };
    // SAFETY: clock_settime reads the timespec we give it.
    unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &time) };
}

/// A line on the machine's console, which the host keeps as the job's console log.
pub fn say(line: &str) {
    eprintln!("cf-supervisor: {line}");
}

/// Makes the console the supervisor's output and nothing its input.
pub fn take_console(console: &File) {
    // SAFETY: dup2 on descriptors we own; a failure leaves the old ones in place.
    unsafe {
        libc::dup2(console.as_raw_fd(), 1);
        libc::dup2(console.as_raw_fd(), 2);
    }
    if let Ok(null) = File::open("/dev/null") {
        // SAFETY: as above.
        unsafe { libc::dup2(null.as_raw_fd(), 0) };
    }
}

/// Writes what is in memory to the disks and turns the machine off. Never returns.
pub fn power_off() -> ! {
    // SAFETY: sync and reboot take no pointers.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// Sends a signal; a process that is gone is no error.
pub fn kill(pid: u32, signal: libc::c_int) {
    if pid > 1 {
        // SAFETY: kill takes no pointers.
        unsafe { libc::kill(pid as libc::pid_t, signal) };
    }
}

/// The bytes a file system has in use.
pub fn disk_used(path: &str) -> u64 {
    let path = cstr(path);
    // SAFETY: statvfs fills the zeroed structure we give it; the path is NUL terminated.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: as above.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return 0;
    }
    (stat.f_blocks as u64)
        .saturating_sub(stat.f_bfree as u64)
        .saturating_mul(stat.f_frsize as u64)
}

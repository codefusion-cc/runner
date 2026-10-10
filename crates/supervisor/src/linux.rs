use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::time::Duration;

const MODULE_LIST: &str = "/etc/modules";
const MODULE_DIR: &str = "/lib/modules";
const CONSOLE: &str = "/dev/hvc0";
/// A VM whose host is gone powers itself off after this long.
const LIFETIME: Duration = Duration::from_secs(120);

pub fn run() -> ! {
    mount("proc", "/proc", "proc");
    mount("sysfs", "/sys", "sysfs");
    mount("devtmpfs", "/dev", "devtmpfs");

    let mut console = wait_for_console();
    let kernel = kernel_release();
    say(&mut console, &format!("cf-supervisor: kernel {kernel}"));

    let mut loaded = Vec::new();
    for name in fs::read_to_string(MODULE_LIST)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.is_empty())
    {
        match load_module(name) {
            Ok(()) => loaded.push(name.to_string()),
            Err(error) => {
                say(
                    &mut console,
                    &format!("cf-supervisor: module {name} failed: {error}"),
                );
                power_off();
            }
        }
    }
    say(
        &mut console,
        &format!("cf-supervisor: modules {}", loaded.join(" ")),
    );
    say(&mut console, "cf-supervisor: ready");

    wait_for_host(&console);
    say(&mut console, "cf-supervisor: poweroff");
    power_off()
}

fn mount(source: &str, target: &str, kind: &str) {
    let (source, target, kind) = (cstr(source), cstr(target), cstr(kind));
    // SAFETY: the three strings are valid and NUL terminated for the call; no data pointer is given.
    unsafe {
        libc::mount(
            source.as_ptr(),
            target.as_ptr(),
            kind.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
}

/// The virtio console appears a moment after devtmpfs is mounted.
fn wait_for_console() -> File {
    for _ in 0..200 {
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(CONSOLE) {
            return file;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    power_off()
}

fn say(console: &mut File, line: &str) {
    let _ = writeln!(console, "{line}");
}

fn kernel_release() -> String {
    fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

fn load_module(name: &str) -> Result<(), String> {
    let path = format!("{MODULE_DIR}/{name}.ko");
    let file = File::open(&path).map_err(|error| format!("{path}: {error}"))?;
    let params = cstr("");
    // SAFETY: finit_module takes a file descriptor we own for the call and a NUL terminated parameter string.
    let result = unsafe { libc::syscall(libc::SYS_finit_module, file.as_raw_fd(), params.as_ptr(), 0) };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EEXIST) {
        Ok(())
    } else {
        Err(error.to_string())
    }
}

/// Blocks until the host sends a line, or until [`LIFETIME`] is over.
fn wait_for_host(console: &File) {
    let mut poll = libc::pollfd {
        fd: console.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd and a length of 1.
    unsafe { libc::poll(&mut poll, 1, LIFETIME.as_millis() as i32) };
    let mut byte = [0u8; 64];
    if poll.revents & libc::POLLIN != 0 {
        let _ = (&*console).read(&mut byte);
    }
}

fn power_off() -> ! {
    // SAFETY: sync and reboot take no pointers.
    unsafe {
        libc::sync();
        libc::reboot(libc::RB_POWER_OFF);
    }
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn cstr(text: &str) -> CString {
    CString::new(text).expect("no NUL in a constant")
}

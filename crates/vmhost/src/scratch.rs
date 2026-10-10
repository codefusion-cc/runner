//! A job's disk: an empty, formatted file system the size the job asked for. Formatting takes a machine and
//! seconds, so it happens once an image and size; every job's disk is a copy-on-write copy of that template,
//! which costs no time and no space until the job writes.

use crate::layout::{Layout, private};
use crate::task::run_task;
use cf_guest_proto::Task;
use cf_vmm::{Disk, Network, VmSpec, Vmm};
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The disk names the guest finds its disks by.
pub const IMAGE_DISK: &str = "cf-image";
/// See [`IMAGE_DISK`].
pub const SCRATCH_DISK: &str = "cf-scratch";

/// The longest formatting a disk may take, the machine's boot and end included.
const FORMAT: Duration = Duration::from_secs(90);

/// The files a guest boots from and runs in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuestFiles {
    /// The kernel.
    pub kernel: PathBuf,
    /// The initramfs with the supervisor.
    pub initramfs: PathBuf,
    /// The image's root file system.
    pub rootfs: PathBuf,
}

fn say(what: &str, error: impl std::fmt::Display) -> String {
    format!("{what}: {error}")
}

/// The template for this image and size, made first if it is not there. Vm-hosts that need the same one at the
/// same time wait for the first to make it. `give_up` is asked many times a second while this waits, for the
/// other vm-host or for the formatting; once it says yes this is an error at once.
pub fn template(
    vmm: &dyn Vmm,
    layout: &Layout,
    files: &GuestFiles,
    image: &str,
    bytes: u64,
    console: &Path,
    give_up: &dyn Fn() -> bool,
) -> Result<PathBuf, String> {
    let folder = layout.scratch_templates();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&folder)
        .map_err(|error| say("the templates' folder", error))?;
    // Every job's disk starts as a copy of what is in here.
    private(&folder)?;
    let path = folder.join(format!("{image}-{bytes}.img"));
    if path.exists() {
        return Ok(path);
    }
    let lock = File::create(folder.join(format!("{image}-{bytes}.lock")))
        .map_err(|error| say("the template's lock", error))?;
    // SAFETY: flock takes no pointers; the descriptor is open for the call. The lock goes with the file.
    while unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(say("the template's lock", error));
        }
        if give_up() {
            return Err("given up while another job made the same disk".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if path.exists() {
        return Ok(path);
    }
    let partial = folder.join(format!("{image}-{bytes}.partial"));
    let _ = fs::remove_file(&partial);
    File::create(&partial)
        .and_then(|file| file.set_len(bytes))
        .map_err(|error| say("the template", error))?;
    let spec = VmSpec {
        kernel: files.kernel.clone(),
        initramfs: files.initramfs.clone(),
        memory_bytes: 1 << 30,
        cpus: 2,
        disks: vec![
            Disk {
                path: files.rootfs.clone(),
                id: IMAGE_DISK.into(),
                read_only: true,
            },
            Disk {
                path: partial.clone(),
                id: SCRATCH_DISK.into(),
                read_only: false,
            },
        ],
        network: Network::None,
        console: console.to_path_buf(),
    };
    let made = run_task(vmm, &spec, Task::FormatScratch, FORMAT, give_up).and_then(|report| {
        match report.summary.exit_code {
            Some(0) => Ok(()),
            code => Err(format!("formatting ended with {code:?}")),
        }
    });
    if let Err(error) = made {
        let _ = fs::remove_file(&partial);
        return Err(say("the job's disk could not be formatted", error));
    }
    fs::rename(&partial, &path).map_err(|error| say("the template", error))?;
    Ok(path)
}

/// Copies a template to a job's own disk. On APFS the copy shares every block until the job writes.
pub fn copy(template: &Path, to: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        if let (Ok(from), Ok(target)) = (
            CString::new(template.as_os_str().as_bytes()),
            CString::new(to.as_os_str().as_bytes()),
        ) {
            // SAFETY: clonefile reads two NUL terminated paths.
            if unsafe { libc::clonefile(from.as_ptr(), target.as_ptr(), 0) } == 0 {
                return Ok(());
            }
        }
    }
    // Another file system: a real copy, which is slow and as large as the template is on disk.
    fs::copy(template, to)
        .map(drop)
        .map_err(|error| say("the job's disk", error))
}

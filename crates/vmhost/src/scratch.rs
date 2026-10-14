//! A job's disk: an empty, formatted file system the size the job asked for. Formatting takes a machine and
//! seconds, so it happens once an image and size; every job's disk is a copy-on-write copy of that template,
//! which costs no time and no space until the job writes.

use crate::layout::{Layout, make_dir, open_private, private};
use crate::task::run_task;
use cf_guest_proto::{CACHE_DISK, IMAGE_DISK, SCRATCH_DISK, Task};
use cf_vmm::{Disk, Network, VmSpec, Vmm};
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

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

impl GuestFiles {
    /// The machine every guest of ours runs in: the image read-only, `scratch` as the one disk it may write.
    pub(crate) fn machine(
        &self,
        scratch: &Path,
        cache: Option<&Path>,
        memory_bytes: u64,
        cpus: usize,
        network: Network,
        console: &Path,
    ) -> VmSpec {
        let cache = cache.map(|path| Disk {
            path: path.to_path_buf(),
            id: CACHE_DISK.into(),
            read_only: false,
        });
        VmSpec {
            kernel: self.kernel.clone(),
            initramfs: self.initramfs.clone(),
            memory_bytes,
            cpus,
            disks: vec![
                Disk {
                    path: self.rootfs.clone(),
                    id: IMAGE_DISK.into(),
                    read_only: true,
                },
                Disk {
                    path: scratch.to_path_buf(),
                    id: SCRATCH_DISK.into(),
                    read_only: false,
                },
            ]
            .into_iter()
            .chain(cache)
            .collect(),
            network,
            console: console.to_path_buf(),
        }
    }
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
    // Every job's disk starts as a copy of what is in here.
    make_dir(&folder).map_err(|error| say("the templates' folder", error))?;
    private(&folder)?;
    let path = folder.join(format!("{image}-{bytes}.img"));
    if path.exists() {
        return Ok(path);
    }
    let lock = open_private(&folder.join(format!("{image}-{bytes}.lock")))
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
    // A job's disk is a copy of this one, mode and all, and holds what the job had: nobody else reads it.
    open_private(&partial)
        .and_then(|file| file.set_len(bytes))
        .map_err(|error| say("the template", error))?;
    let spec = files.machine(&partial, None, 1 << 30, 2, Network::None, console);
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

/// Copies a template to a job's own disk, readable by this user alone, in place of whatever `to` held. On APFS, and
/// on Linux where the file system shares blocks (btrfs, XFS), the copy shares every block until the job writes. On
/// another Linux file system only the template's data is copied and its holes stay holes, so a formatted disk of
/// many gigabytes costs the few megabytes its file system wrote.
pub fn copy(template: &Path, to: &Path) -> Result<(), String> {
    let _ = fs::remove_file(to);
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
    copy_data(template, to).map_err(|error| say("the job's disk", error))
}

/// A copy that shares the template's blocks where the file system can, and otherwise holds its data only.
fn copy_data(template: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt as _;
    let from = fs::File::open(template)?;
    let out = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)?;
    let copied = shared(&from, &out).or_else(|_| sparse_copy(&from, &out));
    if copied.is_err() {
        drop(out);
        let _ = fs::remove_file(to);
    }
    copied
}

/// The template's blocks shared with the copy, on a file system that can.
#[cfg(target_os = "linux")]
fn shared(from: &fs::File, out: &fs::File) -> std::io::Result<()> {
    // SAFETY: FICLONE takes the source's descriptor as its argument; both are open for the call.
    if unsafe { libc::ioctl(out.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// clonefile, the Mac's way, was tried already.
#[cfg(not(target_os = "linux"))]
fn shared(_from: &fs::File, _out: &fs::File) -> std::io::Result<()> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// Copies the template's data and leaves its holes: each range `SEEK_DATA` finds up to the hole `SEEK_HOLE` finds
/// after it, then the length. A file system without those calls has its whole file as data.
pub fn sparse_copy(from: &fs::File, out: &fs::File) -> std::io::Result<()> {
    let length = from.metadata()?.len();
    let mut at = 0;
    while at < length {
        let Some(data) = seek(from, at, libc::SEEK_DATA)? else {
            break;
        };
        let hole = seek(from, data, libc::SEEK_HOLE)?.unwrap_or(length).min(length);
        copy_range(from, out, data, hole)?;
        at = hole;
    }
    // No sync: the copy is one job's disk, and a host that goes down mid-job ends the job anyway.
    out.set_len(length)
}

/// Where `lseek` with `whence` goes from `offset`, or `None` when no data follows (`ENXIO`). A file system that does
/// not know `whence` has data everywhere and its only hole at the end.
fn seek(file: &fs::File, offset: u64, whence: libc::c_int) -> std::io::Result<Option<u64>> {
    let at =
        libc::off_t::try_from(offset).map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: lseek takes no pointers; the descriptor is open for the call.
    let found = unsafe { libc::lseek(file.as_raw_fd(), at, whence) };
    if found >= 0 {
        return Ok(Some(found as u64));
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENXIO) => Ok(None),
        Some(libc::EINVAL) if whence == libc::SEEK_DATA => Ok(Some(offset)),
        Some(libc::EINVAL) => Ok(Some(file.metadata()?.len())),
        _ => Err(error),
    }
}

/// Copies the bytes from `start` up to `end` to the same place in `out`.
fn copy_range(from: &fs::File, out: &fs::File, start: u64, end: u64) -> std::io::Result<()> {
    use std::os::unix::fs::FileExt as _;
    let mut buffer = vec![0u8; (1 << 20).min((end - start) as usize)];
    let mut at = start;
    while at < end {
        let wanted = buffer.len().min((end - at) as usize);
        let read = from.read_at(&mut buffer[..wanted], at)?;
        if read == 0 {
            // The template got shorter while it was copied: what is gone stays a hole.
            break;
        }
        out.write_all_at(&buffer[..read], at)?;
        at += read as u64;
    }
    Ok(())
}

//! What of the Mac's memory one machine holds. Virtualization.framework runs each machine in a process of its own,
//! a child of launchd and not of the process that started it, so that process is found by what only it has open:
//! the machine's kernel, console and writable disks.
//!
//! What it holds is its physical footprint less what macOS compressed or swapped out of it: the guest's memory as
//! far as the guest has touched it and still has in the Mac's memory, and the process's own. The resident size
//! `ps` shows is no measure of it: it counts the guest's memory more than once (6.3 GiB for a machine of 4 GiB).
//!
//! The calls are those `lsof` and `top` make: `libproc` and a task's name port, which a process of the same user
//! gets without any entitlement and which allows reading, nothing else.

use std::ffi::{c_int, c_void};
use std::mem::{MaybeUninit, size_of};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use cf_host_macos::program_of;

/// The program Virtualization.framework runs a machine in.
const MACHINE_PROCESS: &str = "/System/Library/Frameworks/Virtualization.framework/Versions/A/XPCServices/com.apple.Virtualization.VirtualMachine.xpc/Contents/MacOS/com.apple.Virtualization.VirtualMachine";

/// `PROC_PIDFDVNODEINFO` of `<sys/proc_info.h>`: an open file's vnode, without its path.
const PROC_PIDFDVNODEINFO: c_int = 1;
/// `TASK_VM_INFO` of `<mach/task_info.h>`.
const TASK_VM_INFO: u32 = 22;

/// `struct proc_fileinfo` of `<sys/proc_info.h>`.
#[repr(C)]
struct ProcFileInfo {
    openflags: u32,
    status: u32,
    offset: i64,
    kind: i32,
    guardflags: u32,
}

/// `struct vnode_fdinfo` of `<sys/proc_info.h>`.
#[repr(C)]
struct VnodeFdInfo {
    file: ProcFileInfo,
    vnode: libc::vnode_info,
}

/// `struct task_vm_info` of `<mach/task_info.h>` up to its first revision, which added the footprint. The kernel
/// fills as many revisions as the count it is given has room for. Every field up to here already sits where the
/// header's `pack(4)` puts it.
#[repr(C)]
struct TaskVmInfo {
    virtual_size: u64,
    region_count: i32,
    page_size: i32,
    resident_size: u64,
    resident_size_peak: u64,
    device: u64,
    device_peak: u64,
    internal: u64,
    internal_peak: u64,
    external: u64,
    external_peak: u64,
    reusable: u64,
    reusable_peak: u64,
    purgeable_volatile_pmap: u64,
    purgeable_volatile_resident: u64,
    purgeable_volatile_virtual: u64,
    compressed: u64,
    compressed_peak: u64,
    compressed_lifetime: u64,
    phys_footprint: u64,
}

unsafe extern "C" {
    static mach_task_self_: libc::mach_port_t;
    fn task_name_for_pid(target: libc::mach_port_t, pid: c_int, name: *mut libc::mach_port_t) -> c_int;
    fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> c_int;
}

/// One file, as the kernel names it whatever path it was opened by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileId {
    device: u32,
    inode: u64,
}

/// A task's name port: it names the process itself, not its number, so it never reads another process that got the
/// number later. Given back when dropped.
struct Task(libc::mach_port_t);

impl Drop for Task {
    fn drop(&mut self) {
        // SAFETY: the port is a send right this process got from `task_name_for_pid` and gives back once.
        unsafe { mach_port_deallocate(mach_task_self_, self.0) };
    }
}

// SAFETY: a port name is a number valid in the whole process; any thread may use and give it back.
unsafe impl Send for Task {}

/// The process of one program that holds any of some files open: found once, read by its task for as long as it
/// lives, and found again after.
pub struct Holder {
    files: Vec<FileId>,
    program: PathBuf,
    found: Mutex<Option<(c_int, Task)>>,
}

impl Holder {
    /// The process Virtualization.framework runs a machine in, by the machine's own files. Pass only files no other
    /// machine opens: a read-only image several machines share would name any of them.
    pub fn machine(files: &[&Path]) -> Holder {
        Holder::of(Path::new(MACHINE_PROCESS), files)
    }

    /// The process of `program` that holds any of `files` open. A file that cannot be read now is left out.
    pub fn of(program: &Path, files: &[&Path]) -> Holder {
        Holder {
            files: files
                .iter()
                .filter_map(|path| std::fs::metadata(path).ok())
                .map(|meta| FileId {
                    // `dev_t` is 32 bits on macOS, which is what the kernel reports for an open file.
                    device: meta.dev() as u32,
                    inode: meta.ino(),
                })
                .collect(),
            program: program.to_path_buf(),
            found: Mutex::new(None),
        }
    }

    /// The process, while one holds the files.
    pub fn pid(&self) -> Option<i32> {
        self.read().map(|(pid, _)| pid)
    }

    /// What of the Mac's memory the process holds, in bytes: its physical footprint less what macOS compressed or
    /// swapped out of it. `None` when no process of the program holds the files, or it cannot be read.
    pub fn resident(&self) -> Option<u64> {
        self.read()
            .map(|(_, info)| info.phys_footprint.saturating_sub(info.compressed))
    }

    /// The process and its memory now: the one found before while it lives, else the one that holds the files now.
    fn read(&self) -> Option<(c_int, TaskVmInfo)> {
        let mut found = self.found.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some((pid, task)) = found.as_ref()
            && let Some(info) = vm_info(task)
        {
            return Some((*pid, info));
        }
        *found = None;
        if self.files.is_empty() {
            return None;
        }
        let pid = all_pids().into_iter().find(|&pid| self.holds(pid))?;
        let task = name_port(pid)?;
        // The number may have gone to another process between the look and the port: the port names the process
        // that has it now, so that one must be the holder.
        if !self.holds(pid) {
            return None;
        }
        let info = vm_info(&task)?;
        *found = Some((pid, task));
        Some((pid, info))
    }

    /// Whether `pid` runs the program and holds one of the files open.
    fn holds(&self, pid: c_int) -> bool {
        program_of(pid).is_some_and(|path| path == self.program)
            && open_files(pid).any(|file| self.files.contains(&file))
    }
}

/// The name port of a process of this user.
fn name_port(pid: c_int) -> Option<Task> {
    let mut port: libc::mach_port_t = 0;
    // SAFETY: `port` is a place for one port name, which the call fills on success.
    let result = unsafe { task_name_for_pid(mach_task_self_, pid, &mut port) };
    (result == libc::KERN_SUCCESS).then_some(Task(port))
}

/// A task's memory, or `None` once the process is gone.
fn vm_info(task: &Task) -> Option<TaskVmInfo> {
    let mut info = MaybeUninit::<TaskVmInfo>::zeroed();
    let room = (size_of::<TaskVmInfo>() / size_of::<c_int>()) as u32;
    let mut count = room;
    // SAFETY: the buffer has room for `count` integers, the unit `task_info` counts in; the kernel writes no more.
    let result = unsafe { libc::task_info(task.0, TASK_VM_INFO, info.as_mut_ptr().cast(), &mut count) };
    // A kernel that filled less than the footprint's revision gave no footprint.
    // SAFETY: the call succeeded and filled `count` integers, which cover the whole structure.
    (result == libc::KERN_SUCCESS && count >= room).then(|| unsafe { info.assume_init() })
}

/// Every process there is now.
fn all_pids() -> Vec<c_int> {
    // SAFETY: a null buffer asks for the count only.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    // Room for processes that start between the two calls.
    let mut pids: Vec<c_int> = vec![0; count + 64];
    let bytes = c_int::try_from(pids.len() * size_of::<c_int>()).unwrap_or(c_int::MAX);
    // SAFETY: the buffer has room for `bytes` bytes.
    let filled = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), bytes) };
    pids.truncate(usize::try_from(filled).unwrap_or(0));
    pids.retain(|&pid| pid > 0);
    pids
}

/// The files a process has open. Nothing when it cannot be read.
fn open_files(pid: c_int) -> impl Iterator<Item = FileId> {
    // SAFETY: a null buffer asks for the size the list needs.
    let needed = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    let mut fds: Vec<libc::proc_fdinfo> = Vec::new();
    if let Ok(needed) = usize::try_from(needed) {
        // Room for files opened between the two calls.
        let room = needed / size_of::<libc::proc_fdinfo>() + 16;
        fds = Vec::with_capacity(room);
        let bytes = c_int::try_from(room * size_of::<libc::proc_fdinfo>()).unwrap_or(c_int::MAX);
        // SAFETY: the buffer has room for `bytes` bytes, and `proc_fdinfo` is plain data.
        let filled =
            unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), bytes) };
        let filled = usize::try_from(filled).unwrap_or(0) / size_of::<libc::proc_fdinfo>();
        // SAFETY: the kernel wrote `filled` whole entries, no more than the capacity.
        unsafe { fds.set_len(filled.min(room)) };
    }
    fds.into_iter()
        .filter(|fd| fd.proc_fdtype == libc::PROX_FDTYPE_VNODE as u32)
        .filter_map(move |fd| {
            let mut info = MaybeUninit::<VnodeFdInfo>::zeroed();
            let size = size_of::<VnodeFdInfo>() as c_int;
            // SAFETY: the buffer is a `vnode_fdinfo`, the structure this flavor fills, and is `size` bytes long.
            let filled = unsafe {
                libc::proc_pidfdinfo(
                    pid,
                    fd.proc_fd,
                    PROC_PIDFDVNODEINFO,
                    info.as_mut_ptr().cast(),
                    size,
                )
            };
            (filled == size).then(|| {
                // SAFETY: the kernel filled all of it.
                let stat = unsafe { info.assume_init() }.vnode.vi_stat;
                FileId {
                    device: stat.vst_dev,
                    inode: stat.vst_ino,
                }
            })
        })
}

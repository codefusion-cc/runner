use std::ffi::CString;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use cf_budget::{Host, HostError, HostFacts, HostReading, Now};

use crate::parse::CpuTicks;
use crate::{Raw, read, reading};

fn errno() -> i64 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0).into()
}

/// A Linux host. It only reads: nothing it does changes a setting, a file or another process.
#[derive(Debug)]
pub struct LinuxHost {
    root: PathBuf,
    disk: CString,
}

impl LinuxHost {
    /// This host. `disk` is a folder on the file system that holds the job disks, the state folder; it must exist.
    pub fn new(disk: &Path) -> Result<LinuxHost, HostError> {
        LinuxHost::under(Path::new("/"), disk)
    }

    /// A host whose `/proc` and `/sys` are under `root`: a folder of fixtures in tests.
    pub fn under(root: &Path, disk: &Path) -> Result<LinuxHost, HostError> {
        let disk = CString::new(disk.as_os_str().as_bytes()).map_err(|_| HostError::Implausible {
            what: "the path of the job disks",
            value: disk.display().to_string(),
        })?;
        let host = LinuxHost {
            root: root.to_path_buf(),
            disk,
        };
        host.statvfs()?;
        Ok(host)
    }

    /// What Linux answers now, before anything is made of it.
    pub fn raw(&self) -> Result<Raw, HostError> {
        let memory = read::meminfo(&self.root)?;
        let stats = self.statvfs()?;
        Ok(Raw {
            total_bytes: memory.total_bytes,
            available_bytes: memory.available_bytes,
            swap_total_bytes: memory.swap_total_bytes,
            swap_free_bytes: memory.swap_free_bytes,
            swapouts: read::swapouts(&self.root)?,
            memory_pressure: read::memory_pressure(&self.root)?,
            zones: read::zones(&self.root),
            supplies: read::supplies(&self.root),
            disk_free_bytes: blocks(stats.f_bavail, stats.f_frsize),
        })
    }

    /// The size of the file system with the job disks, in bytes; `None` when Linux will not say.
    pub fn disk_total_bytes(&self) -> Option<u64> {
        let stats = self.statvfs().ok()?;
        Some(blocks(stats.f_blocks, stats.f_frsize))
    }

    /// The time all CPUs spent since the host started, in ticks (`/proc/stat`). Two counts make a share, see
    /// [`cpu_percent`](crate::cpu_percent). `None` when Linux will not say.
    pub fn cpu_ticks(&self) -> Option<CpuTicks> {
        read::cpu_ticks(&self.root)
    }

    /// What Linux says of the file system with the job disks.
    fn statvfs(&self) -> Result<libc::statvfs, HostError> {
        let mut stats = MaybeUninit::<libc::statvfs>::zeroed();
        // SAFETY: the path ends with a NUL and `stats` is a statvfs this call may write.
        let status = unsafe { libc::statvfs(self.disk.as_ptr(), stats.as_mut_ptr()) };
        if status != 0 {
            return Err(HostError::Call {
                what: "statvfs",
                code: errno(),
            });
        }
        // SAFETY: the call succeeded, so it filled the struct in.
        Ok(unsafe { stats.assume_init() })
    }
}

/// So many blocks of `statvfs`'s fragment size, in bytes. The two are 64-bit on Linux and narrower on a Mac.
#[allow(clippy::useless_conversion)]
fn blocks(count: libc::fsblkcnt_t, size: libc::c_ulong) -> u64 {
    u64::from(count).saturating_mul(u64::from(size))
}

impl Host for LinuxHost {
    /// `MemTotal`, and the CPUs that are online. The fast ones of a board whose cores differ are
    /// [`fast_cpus`](crate::fast_cpus).
    fn facts(&self) -> Result<HostFacts, HostError> {
        let total_bytes = read::meminfo(&self.root)?.total_bytes;
        let online = read::online_cpus(&self.root)?;
        let cores = u32::try_from(online.len()).map_err(|_| HostError::Implausible {
            what: "the online CPUs",
            value: online.len().to_string(),
        })?;
        Ok(HostFacts { total_bytes, cores })
    }

    /// `CLOCK_MONOTONIC` stands still while the host is suspended, `CLOCK_BOOTTIME` does not ([`cf_clock::now`]).
    fn now(&self) -> Now {
        cf_clock::now()
    }

    fn read(&mut self) -> Result<HostReading, HostError> {
        let at = self.now();
        let raw = self.raw()?;
        reading(at, &raw)
    }
}

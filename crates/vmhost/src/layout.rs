//! Where a vm-host's files are in the state folder.

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// Writes a file that did not exist, readable and writable by this user only.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .and_then(|mut file| file.write_all(bytes))
}

/// Refuses a file or folder another user could change: one that is not this user's, or that its group or
/// everybody may write to. The error says which and why.
pub(crate) fn private(path: &Path) -> Result<(), String> {
    let found = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    if found.uid() != me {
        return Err(format!(
            "{} belongs to another user (uid {}), so what is in it cannot be trusted",
            path.display(),
            found.uid()
        ));
    }
    if found.mode() & 0o022 != 0 {
        return Err(format!(
            "{} can be written by other users (mode {:o}), so what is in it cannot be trusted",
            path.display(),
            found.mode() & 0o7777
        ));
    }
    Ok(())
}

/// The state folder's layout. Every name that goes into a path was checked to be plain by [`crate::JobRequest`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout(PathBuf);

impl Layout {
    /// The layout of this state folder.
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self(state_dir.as_ref().to_path_buf())
    }

    /// `run/`: one state file, one socket and one folder a running job.
    pub fn run(&self) -> PathBuf {
        self.0.join("run")
    }

    /// The job's state file.
    pub fn state(&self, job: &str) -> PathBuf {
        self.run().join(format!("{job}.json"))
    }

    /// The job's control socket.
    pub fn socket(&self, job: &str) -> PathBuf {
        self.run().join(format!("{job}.sock"))
    }

    /// The folder of the job's machine: its kernel, its disk. Gone with the job.
    pub fn job_dir(&self, job: &str) -> PathBuf {
        self.run().join(job)
    }

    /// The job's console log, which outlives it.
    pub fn console(&self, job: &str) -> PathBuf {
        self.0.join("logs").join(format!("{job}.console.log"))
    }

    /// The root file system of an installed image.
    pub fn rootfs(&self, image: &str) -> PathBuf {
        self.0.join("images").join(image).join("rootfs.img")
    }

    /// `scratch/`: one formatted, empty disk an image and size, which every job's disk is a copy of.
    pub fn scratch_templates(&self) -> PathBuf {
        self.0.join("scratch")
    }
}

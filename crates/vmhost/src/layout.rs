//! Where a vm-host's files are in the state folder.

use cf_image::store::Store;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
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

/// Opens a file for writing that this user alone reads and writes, making it when it is not there.
pub(crate) fn open_private(path: &Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
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

/// Makes a folder for this user alone, with the folders above it. One that is there already stays as it is:
/// [`private`] says whether it can be trusted.
pub(crate) fn make_dir(path: &Path) -> std::io::Result<()> {
    fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
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
        self.logs().join(format!("{job}.console.log"))
    }

    /// `images/`: the installed images, a [`cf_image::store::Store`].
    pub fn images(&self) -> PathBuf {
        self.0.join("images")
    }

    /// The root file system of an installed image.
    pub fn rootfs(&self, image: &str) -> PathBuf {
        Store::new(self.images()).rootfs_path(image)
    }

    /// What a job trusts when it boots from an installed image: the root file system, its folder, the images'
    /// folder and the state folder. The image was verified when it was installed, which holds for as long as
    /// nobody else can write to any of these.
    pub(crate) fn trusted_for(&self, image: &str) -> [PathBuf; 4] {
        let images = self.images();
        [self.rootfs(image), images.join(image), images, self.0.clone()]
    }

    /// `logs/`: the console logs.
    pub fn logs(&self) -> PathBuf {
        self.0.join("logs")
    }

    /// `machines/`: on Linux, the monitor's own folder of each running machine (its vsock socket, its events).
    pub fn machines(&self) -> PathBuf {
        self.0.join("machines")
    }

    /// `programs/`: on Linux, cloud-hypervisor and pasta as the Runner carries them, unpacked.
    pub fn programs(&self) -> PathBuf {
        self.0.join("programs")
    }

    /// `scratch/`: one formatted, empty disk an image and size, which every job's disk is a copy of.
    pub fn scratch_templates(&self) -> PathBuf {
        self.0.join("scratch")
    }
}

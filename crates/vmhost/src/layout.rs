//! Where a vm-host's files are in the state folder.

use std::path::{Path, PathBuf};

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

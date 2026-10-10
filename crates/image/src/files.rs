use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::manifest::valid_name;

/// Where the files of a release come from: a folder, memory, or later a download.
pub trait Files {
    /// Opens `name` for reading. A file that does not exist must be an [`io::ErrorKind::NotFound`] error.
    fn open(&self, name: &str) -> io::Result<Box<dyn Read + '_>>;
}

/// The files of a release in a folder.
pub struct DirFiles(PathBuf);

impl DirFiles {
    /// A folder of release files.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self(dir.as_ref().to_path_buf())
    }
}

impl Files for DirFiles {
    /// Opens a plain file of the folder itself: a name with a folder in it, a symbolic link and anything that is
    /// not a regular file are errors, so the bytes read are the folder's own and no pipe can stall the reader.
    fn open(&self, name: &str) -> io::Result<Box<dyn Read + '_>> {
        if !valid_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a plain file name",
            ));
        }
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let file = options.open(self.0.join(name))?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a regular file"));
        }
        Ok(Box::new(file))
    }
}

/// The files of a release held in memory (what a binary embeds, and what tests tamper with).
#[derive(Default, Clone)]
pub struct MemFiles(BTreeMap<String, Vec<u8>>);

impl MemFiles {
    /// An empty set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or replaces a file.
    pub fn insert(&mut self, name: &str, bytes: impl Into<Vec<u8>>) {
        self.0.insert(name.to_string(), bytes.into());
    }

    /// Removes a file.
    pub fn remove(&mut self, name: &str) {
        self.0.remove(name);
    }

    /// A file's bytes.
    pub fn get(&self, name: &str) -> Option<&[u8]> {
        self.0.get(name).map(Vec::as_slice)
    }

    /// A file's bytes, for tests that change one.
    pub fn get_mut(&mut self, name: &str) -> Option<&mut Vec<u8>> {
        self.0.get_mut(name)
    }
}

impl Files for MemFiles {
    fn open(&self, name: &str) -> io::Result<Box<dyn Read + '_>> {
        match self.0.get(name) {
            Some(bytes) => Ok(Box::new(bytes.as_slice())),
            None => Err(io::ErrorKind::NotFound.into()),
        }
    }
}

//! The images installed on this machine: a release is copied into a folder only the Runner writes to, verified
//! there, and its root file system joined from its parts. What a job's machine is given is what was verified.
//!
//! The store remembers the build time of the newest release it accepted and refuses an older one from then on,
//! so a machine cannot be talked back into a release with a known hole. Downloading a release is not here: this
//! takes a folder that has the release's files.

use crate::{DirFiles, ImageError, Manifest, verify};
use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// The name of an installed image's root file system in its folder.
pub const ROOTFS_NAME: &str = "rootfs.img";
const ACCEPTED_NAME: &str = "accepted.json";

/// Why a release was not installed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The version is not a plain name, so it cannot name a folder.
    #[error("`{0}` is not a version: letters, digits, dots and dashes only")]
    Version(String),
    /// The release did not verify.
    #[error("the release is refused: {0}")]
    Refused(#[from] ImageError),
    /// The release is genuine but has no root file system (it is the bundle a binary embeds).
    #[error("the release has no root file system")]
    NoRootfs,
    /// A file could not be read or written.
    #[error("{0}: {1}")]
    Io(String, io::Error),
}

/// The folder of installed images.
#[derive(Debug, Clone)]
pub struct Store(PathBuf);

fn plain(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 64
        && version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        && !version.starts_with('.')
}

impl Store {
    /// The store in this folder, which need not exist yet.
    pub fn new(dir: impl AsRef<Path>) -> Self {
        Self(dir.as_ref().to_path_buf())
    }

    /// The root file system of an installed image, if that version is installed.
    pub fn rootfs(&self, version: &str) -> Option<PathBuf> {
        let path = self.0.join(version).join(ROOTFS_NAME);
        (plain(version) && path.is_file()).then_some(path)
    }

    /// The build time of the newest release this store accepted; 0 before the first.
    pub fn newest_accepted(&self) -> u64 {
        fs::read_to_string(self.0.join(ACCEPTED_NAME))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }

    /// Installs the release in `source` as `version` and returns its manifest. Every file is copied first, then
    /// checked where nobody else can change it: the signature against `key`, the version, each hash and size, and
    /// that the release is not older than one accepted before. A release already installed is verified again all
    /// the same; a refused one leaves nothing behind.
    pub fn install(&self, source: &Path, version: &str, key: &VerifyingKey) -> Result<Manifest, StoreError> {
        if !plain(version) {
            return Err(StoreError::Version(version.to_string()));
        }
        let partial = self.0.join(format!("{version}.partial"));
        let result = self.install_into(&partial, source, version, key);
        if result.is_err() {
            let _ = fs::remove_dir_all(&partial);
        }
        result
    }

    fn install_into(
        &self,
        partial: &Path,
        source: &Path,
        version: &str,
        key: &VerifyingKey,
    ) -> Result<Manifest, StoreError> {
        use std::os::unix::fs::DirBuilderExt as _;
        let io = |path: &Path| {
            let path = path.display().to_string();
            move |error| StoreError::Io(path, error)
        };
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.0)
            .map_err(io(&self.0))?;
        let _ = fs::remove_dir_all(partial);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(partial)
            .map_err(io(partial))?;
        for entry in fs::read_dir(source).map_err(io(source))? {
            let entry = entry.map_err(io(source))?;
            // Files of the folder itself only: a link could name a file that changes after it was checked.
            if entry.file_type().map_err(io(&entry.path()))?.is_file() {
                fs::copy(entry.path(), partial.join(entry.file_name())).map_err(io(&entry.path()))?;
            }
        }
        let manifest = verify(&DirFiles::new(partial), key, version, self.newest_accepted())?.manifest;
        let rootfs = manifest.rootfs.as_ref().ok_or(StoreError::NoRootfs)?;

        // The parts are joined under the hash of the whole, so the file a machine boots from is the one signed.
        let joined = partial.join(ROOTFS_NAME);
        let mut out = File::create(&joined).map_err(io(&joined))?;
        let mut hash = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        for part in &rootfs.parts {
            let path = partial.join(&part.name);
            let mut file = File::open(&path).map_err(io(&path))?;
            loop {
                let read = file.read(&mut buffer).map_err(io(&path))?;
                if read == 0 {
                    break;
                }
                hash.update(&buffer[..read]);
                out.write_all(&buffer[..read]).map_err(io(&joined))?;
            }
            fs::remove_file(&path).map_err(io(&path))?;
        }
        out.sync_all().map_err(io(&joined))?;
        if format!("{:x}", hash.finalize()) != rootfs.sha256 {
            return Err(StoreError::Refused(ImageError::HashMismatch(ROOTFS_NAME.into())));
        }

        let installed = self.0.join(version);
        let _ = fs::remove_dir_all(&installed);
        fs::rename(partial, &installed).map_err(io(&installed))?;
        if manifest.built > self.newest_accepted() {
            let accepted = self.0.join(ACCEPTED_NAME);
            let new = self.0.join(format!("{ACCEPTED_NAME}.new"));
            fs::write(&new, manifest.built.to_string())
                .and_then(|()| fs::rename(&new, &accepted))
                .map_err(io(&accepted))?;
        }
        Ok(manifest)
    }
}

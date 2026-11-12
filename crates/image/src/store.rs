//! The images installed on this machine: a release is copied into a folder only the Runner writes to, verified
//! there, and its root file system joined from its parts. What a job's machine is given is what was verified. On a
//! host in container mode the joined root file system goes to the container engine instead ([`Target::Engine`]),
//! which holds it as an image named after the version once its hash is the signed one; the folder keeps the rest.
//!
//! The store remembers the build time of the newest release it accepted and refuses an older one from then on,
//! so a machine cannot be talked back into a release with a known hole. Downloading a release is not here: this
//! takes a folder that has the release's files.

use crate::{
    Asset, DirFiles, ImageError, MANIFEST_NAME, Manifest, RootfsKind, SIGNATURE_NAME, SUMS_NAME, verify,
};
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
    /// The store's record of the newest release it accepted is there but is no build time. Nothing is installed
    /// until it is put right: read as "none yet", it would let every old release back in.
    #[error("{0} does not hold the build time of the newest release accepted: {1}")]
    Accepted(String, String),
    /// The release's root file system is not of the kind this host installs.
    #[error(
        "the release's root file system is {}, and this host installs {}: a container-image release is for hosts in \
         container mode, an image release for the others",
        .found.name(),
        .wanted.name()
    )]
    Kind {
        /// What the release holds.
        found: RootfsKind,
        /// What the host installs.
        wanted: RootfsKind,
    },
    /// The container engine did not take the image.
    #[error("the container engine: {0}")]
    Engine(String),
    /// A file could not be read or written.
    #[error("{0}: {1}")]
    Io(String, io::Error),
}

/// Where an installed image's root file system goes.
pub enum Target<'a> {
    /// A file in the image's folder, which machines boot from: an erofs image.
    File,
    /// A container engine, which imports the tar as an image named after the version (container mode).
    Engine(&'a mut dyn Engine),
}

/// A container engine's images, as the store uses them.
pub trait Engine {
    /// Imports this gzip tar of `size` bytes as an image with no name yet and returns its id. It reads the whole of
    /// it; the store checks its hash after.
    fn import(&mut self, tar: &mut dyn Read, size: u64) -> Result<String, String>;
    /// Names an imported image after the version, in place of any image of that name before.
    fn name(&mut self, id: &str, version: &str) -> Result<(), String>;
    /// Removes an imported image; what is not there is not an error.
    fn remove(&mut self, id: &str);
}

/// The parts of a root file system read as one, hashed as they are read.
struct Joined<'a> {
    folder: &'a Path,
    parts: std::slice::Iter<'a, Asset>,
    file: Option<(File, PathBuf)>,
    hash: Sha256,
    read: u64,
}

impl Read for Joined<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.file.is_none() {
                let Some(part) = self.parts.next() else {
                    return Ok(0);
                };
                let path = self.folder.join(&part.name);
                self.file = Some((File::open(&path)?, path));
            }
            let Some((file, _)) = self.file.as_mut() else {
                return Ok(0);
            };
            let read = file.read(buffer)?;
            if read == 0 {
                self.file = None;
                continue;
            }
            self.hash.update(&buffer[..read]);
            self.read += read as u64;
            return Ok(read);
        }
    }
}

/// The folder of installed images.
#[derive(Debug, Clone)]
pub struct Store(PathBuf);

/// Whether this can be an installed image's version: 1 to 64 letters, digits, dots and dashes, not starting with a
/// dot. It names a folder, and whoever asks for an image by version checks the name with this first.
pub fn is_version(version: &str) -> bool {
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
        let path = self.rootfs_path(version);
        (is_version(version) && path.is_file()).then_some(path)
    }

    /// Where the root file system of this version is once it is installed. The version is a name
    /// [`is_version`] takes.
    pub fn rootfs_path(&self, version: &str) -> PathBuf {
        self.0.join(version).join(ROOTFS_NAME)
    }

    /// The folder of an installed image, if that version is installed, whatever its root file system's target.
    pub fn installed(&self, version: &str) -> Option<PathBuf> {
        let folder = self.0.join(version);
        (is_version(version) && folder.join(MANIFEST_NAME).is_file()).then_some(folder)
    }

    /// The build time of the newest release this store accepted; 0 before the first, when there is no record.
    /// A record that cannot be read, or holds something else than one number, is an error.
    pub fn newest_accepted(&self) -> Result<u64, StoreError> {
        let path = self.0.join(ACCEPTED_NAME);
        let refused = |why: String| StoreError::Accepted(path.display().to_string(), why);
        match fs::read_to_string(&path) {
            Ok(text) => text.trim().parse().map_err(|_| refused("not a number".into())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(refused(error.to_string())),
        }
    }

    /// Installs the release in `source` as `version` and returns its manifest. Every file is copied first, then
    /// checked where nobody else can change it: the signature against `key`, the version, each hash and size, and
    /// that the release is not older than one accepted before. What the folder holds beyond the files the signed
    /// list names is not installed. A release already installed is verified again all the same; a refused one
    /// leaves nothing behind. One install runs in a store at a time: a second one waits for the first.
    pub fn install(&self, source: &Path, version: &str, key: &VerifyingKey) -> Result<Manifest, StoreError> {
        self.install_to(source, version, key, Target::File)
    }

    /// [`Store::install`], with the root file system going to `target`, which must take the release's kind: a file
    /// an erofs image, an engine a tar. An image the engine imported is named only once all of the release is in
    /// place, and removed when it is not.
    pub fn install_to(
        &self,
        source: &Path,
        version: &str,
        key: &VerifyingKey,
        target: Target<'_>,
    ) -> Result<Manifest, StoreError> {
        if !is_version(version) {
            return Err(StoreError::Version(version.to_string()));
        }
        use std::os::unix::fs::DirBuilderExt as _;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.0)
            .map_err(|error| StoreError::Io(self.0.display().to_string(), error))?;
        // Two installs would share the work folder, and one could put the other's unchecked files in place.
        let lock = File::open(&self.0)
            .and_then(|folder| folder.lock().map(|()| folder))
            .map_err(|error| StoreError::Io(self.0.display().to_string(), error))?;
        // No version starts with a dot, so the store's own folders are never an installed image's.
        let partial = self.0.join(format!(".{version}.partial"));
        let result = self.install_into(&partial, source, version, key, target);
        drop(lock);
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
        mut target: Target<'_>,
    ) -> Result<Manifest, StoreError> {
        use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
        let io = |path: &Path| {
            let path = path.display().to_string();
            move |error| StoreError::Io(path, error)
        };
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
        let newest = self.newest_accepted()?;
        let manifest = verify(&DirFiles::new(partial), key, version, newest)?.manifest;
        manifest.for_this_host()?;
        let rootfs = manifest.rootfs.as_ref().ok_or(StoreError::NoRootfs)?;
        let wanted = match target {
            Target::File => RootfsKind::Erofs,
            Target::Engine(_) => RootfsKind::TarGz,
        };
        if rootfs.kind != wanted {
            return Err(StoreError::Kind {
                found: rootfs.kind,
                wanted,
            });
        }

        // Only what was checked stays: a file the signed list does not name was read by nobody.
        let mut checked = vec![MANIFEST_NAME, SUMS_NAME, SIGNATURE_NAME];
        checked.extend(manifest.assets().into_iter().map(|asset| asset.name.as_str()));
        for entry in fs::read_dir(partial).map_err(io(partial))? {
            let entry = entry.map_err(io(partial))?;
            if !checked.iter().any(|name| entry.file_name() == *name) {
                fs::remove_file(entry.path()).map_err(io(&entry.path()))?;
            }
        }

        // The parts are joined under the hash of the whole, so what a machine boots from, or a container is made
        // from, is what was signed.
        let mut joined = Joined {
            folder: partial,
            parts: rootfs.parts.iter(),
            file: None,
            hash: Sha256::new(),
            read: 0,
        };
        let imported = match &mut target {
            Target::File => {
                // New and its owner's alone, whatever this process's mask for new files is.
                let path = partial.join(ROOTFS_NAME);
                let mut out = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)
                    .map_err(io(&path))?;
                io::copy(&mut joined, &mut out).map_err(io(&path))?;
                out.sync_all().map_err(io(&path))?;
                None
            }
            Target::Engine(engine) => Some(
                engine
                    .import(&mut joined, rootfs.size)
                    .map_err(StoreError::Engine)?,
            ),
        };
        let Joined { hash, read, .. } = joined;
        let result = (|| {
            if read != rootfs.size || format!("{:x}", hash.finalize()) != rootfs.sha256 {
                return Err(StoreError::Refused(ImageError::HashMismatch(ROOTFS_NAME.into())));
            }
            for part in &rootfs.parts {
                let path = partial.join(&part.name);
                fs::remove_file(&path).map_err(io(&path))?;
            }
            self.put_in_place(partial, version, &manifest, newest, |version| {
                match (&mut target, &imported) {
                    (Target::Engine(engine), Some(id)) => {
                        engine.name(id, version).map_err(StoreError::Engine)
                    }
                    _ => Ok(()),
                }
            })
        })();
        if result.is_err()
            && let (Target::Engine(engine), Some(id)) = (&mut target, &imported)
        {
            engine.remove(id);
        }
        result.map(|()| manifest)
    }

    /// Records the release as the newest accepted, names what the engine holds of it, and moves its folder in.
    fn put_in_place(
        &self,
        partial: &Path,
        version: &str,
        manifest: &Manifest,
        newest: u64,
        name: impl FnOnce(&str) -> Result<(), StoreError>,
    ) -> Result<(), StoreError> {
        let io = |path: &Path| {
            let path = path.display().to_string();
            move |error| StoreError::Io(path, error)
        };

        // The record moves first: a release in place without it could be followed by one older than itself.
        if manifest.built > newest {
            let accepted = self.0.join(ACCEPTED_NAME);
            let new = self.0.join(format!("{ACCEPTED_NAME}.new"));
            // On the disk before it takes the old record's place: a record cut short by a power loss would
            // refuse every release from then on.
            File::create(&new)
                .and_then(|mut file| {
                    file.write_all(manifest.built.to_string().as_bytes())?;
                    file.sync_all()
                })
                .and_then(|()| fs::rename(&new, &accepted))
                .map_err(io(&accepted))?;
        }
        name(version)?;
        let installed = self.0.join(version);
        // A version installed again is put aside, not deleted first: when the new one cannot take its place the old
        // one comes back.
        let aside = self.0.join(format!(".{version}.old"));
        let _ = fs::remove_dir_all(&aside);
        let had_one = fs::rename(&installed, &aside).is_ok();
        if let Err(error) = fs::rename(partial, &installed) {
            if had_one {
                let _ = fs::rename(&aside, &installed);
            }
            return Err(io(&installed)(error));
        }
        let _ = fs::remove_dir_all(&aside);
        Ok(())
    }
}

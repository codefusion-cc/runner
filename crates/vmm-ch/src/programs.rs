//! cloud-hypervisor and pasta as the Runner carries them ([`crate::pins`]): unpacked into a private folder of the
//! state folder and checked byte for byte before every machine, so what runs is what was built in.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{ErrorKind, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, OpenOptionsExt as _};
use std::path::{Path, PathBuf};

/// The programs a build carries.
#[derive(Clone, Copy)]
pub struct Embedded {
    /// cloud-hypervisor's static aarch64 build.
    pub cloud_hypervisor: &'static [u8],
    /// Its SHA-256, the pin.
    pub cloud_hypervisor_sha256: &'static str,
    /// pasta, built statically from the pinned source.
    pub pasta: &'static [u8],
    /// Its SHA-256, as the build found it.
    pub pasta_sha256: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// The programs this build carries, if it carries them.
pub fn embedded() -> Option<Embedded> {
    EMBEDDED
}

/// Where the unpacked programs are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Programs {
    /// cloud-hypervisor.
    pub cloud_hypervisor: PathBuf,
    /// pasta. Its name must be `pasta`: the same program is passt under another.
    pub pasta: PathBuf,
}

/// Unpacks the embedded programs into `dir` (made private when it is not there), each checked against its
/// SHA-256; a file that is there and right stays. A folder or file another user could change is refused.
pub fn unpack(dir: &Path) -> Result<Programs, String> {
    let embedded = embedded().ok_or(
        "this build carries no cloud-hypervisor and pasta: build it after `cargo xtask vmm-programs` on Linux",
    )?;
    unpack_these(dir, &embedded)
}

fn unpack_these(dir: &Path, embedded: &Embedded) -> Result<Programs, String> {
    let say = |path: &Path, error: std::io::Error| format!("{}: {error}", path.display());
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| say(dir, e))?;
    owned_by_me(dir)?;
    let programs = Programs {
        cloud_hypervisor: dir.join("cloud-hypervisor"),
        pasta: dir.join("pasta"),
    };
    for (path, bytes, sha256) in [
        (
            &programs.cloud_hypervisor,
            embedded.cloud_hypervisor,
            embedded.cloud_hypervisor_sha256,
        ),
        (&programs.pasta, embedded.pasta, embedded.pasta_sha256),
    ] {
        if check(path, sha256).is_ok() {
            continue;
        }
        let partial = path.with_extension("partial");
        let _ = fs::remove_file(&partial);
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o500)
            .open(&partial)
            .and_then(|mut file| file.write_all(bytes).and_then(|()| file.sync_all()))
            .map_err(|e| say(&partial, e))?;
        fs::rename(&partial, path).map_err(|e| say(path, e))?;
        check(path, sha256)?;
    }
    Ok(programs)
}

/// Whether the file at `path` is this user's, writable by nobody else, and has the SHA-256 `sha256`.
pub fn check(path: &Path, sha256: &str) -> Result<(), String> {
    owned_by_me(path)?;
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let found = format!("{:x}", Sha256::digest(&bytes));
    if found != sha256 {
        return Err(format!("{} has SHA-256 {found}, not {sha256}", path.display()));
    }
    Ok(())
}

fn owned_by_me(path: &Path) -> Result<(), String> {
    let found = fs::symlink_metadata(path).map_err(|e| match e.kind() {
        ErrorKind::NotFound => format!("{} is not there", path.display()),
        _ => format!("{}: {e}", path.display()),
    })?;
    // SAFETY: geteuid takes nothing and cannot fail.
    let me = unsafe { libc::geteuid() };
    if found.file_type().is_symlink() || found.uid() != me || found.mode() & 0o022 != 0 {
        return Err(format!(
            "{} is not this user's alone, so it cannot be trusted",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn sha(bytes: &[u8]) -> &'static str {
        Box::leak(format!("{:x}", Sha256::digest(bytes)).into_boxed_str())
    }

    fn fake() -> Embedded {
        Embedded {
            cloud_hypervisor: b"a monitor",
            cloud_hypervisor_sha256: sha(b"a monitor"),
            pasta: b"a network",
            pasta_sha256: sha(b"a network"),
        }
    }

    #[test]
    fn the_programs_are_unpacked_private_and_runnable() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("bin");
        let programs = unpack_these(&dir, &fake()).unwrap();
        assert_eq!(fs::read(&programs.cloud_hypervisor).unwrap(), b"a monitor");
        assert_eq!(fs::read(&programs.pasta).unwrap(), b"a network");
        assert_eq!(programs.pasta.file_name().unwrap(), "pasta");
        assert_eq!(fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(&programs.pasta).unwrap().permissions().mode() & 0o777,
            0o500
        );
        assert!(!dir.join("pasta.partial").exists());
    }

    #[test]
    fn a_changed_program_is_put_back_and_a_right_one_stays() {
        let folder = tempfile::tempdir().unwrap();
        let programs = unpack_these(folder.path(), &fake()).unwrap();
        let before = fs::metadata(&programs.pasta).unwrap().modified().unwrap();
        fs::set_permissions(&programs.cloud_hypervisor, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&programs.cloud_hypervisor, b"something else").unwrap();
        unpack_these(folder.path(), &fake()).unwrap();
        assert_eq!(fs::read(&programs.cloud_hypervisor).unwrap(), b"a monitor");
        assert_eq!(fs::metadata(&programs.pasta).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn a_link_in_place_of_a_program_is_replaced_not_followed() {
        let folder = tempfile::tempdir().unwrap();
        let elsewhere = folder.path().join("elsewhere");
        fs::write(&elsewhere, b"a monitor").unwrap();
        let dir = folder.path().join("bin");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("cloud-hypervisor")).unwrap();
        let programs = unpack_these(&dir, &fake()).unwrap();
        assert!(
            !fs::symlink_metadata(&programs.cloud_hypervisor)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read(&elsewhere).unwrap(), b"a monitor");
    }

    #[test]
    fn a_folder_others_may_write_to_is_refused() {
        let folder = tempfile::tempdir().unwrap();
        let dir = folder.path().join("bin");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
        let refused = unpack_these(&dir, &fake()).unwrap_err();
        assert!(refused.contains("cannot be trusted"), "{refused}");
    }

    #[test]
    fn a_check_says_what_differs() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("p");
        fs::write(&path, b"x").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
        assert!(check(&path, sha(b"x")).is_ok());
        let refused = check(&path, sha(b"y")).unwrap_err();
        assert!(refused.contains("SHA-256"), "{refused}");
        assert!(
            check(&folder.path().join("missing"), sha(b"x"))
                .unwrap_err()
                .contains("not there")
        );
    }
}

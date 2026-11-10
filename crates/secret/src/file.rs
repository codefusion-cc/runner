//! Files that hold a secret or state that only this user may read.

use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _};
use std::path::Path;

/// Makes a folder for this user alone (mode 0700), with the folders above it. One that is there already stays as it
/// is.
pub fn private_dir(path: &Path) -> std::io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Writes a file for this user alone and puts it in place in one step, so that a reader never sees half of it.
///
/// The bytes go to a file of their own beside `path`, `.<name>.<pid>.<n>.partial` (mode 0600, synced), which is
/// renamed over `path`: writers in several processes or threads at once each put a whole file in place, the last
/// one's staying. A crash leaves the old file whole, or the new one, and at worst a `.partial` file beside it. The
/// folder must exist.
pub fn replace_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    replace_private_as(path, bytes, 0o600)
}

/// [`replace_private`] with the file's mode `mode`, of which only the user's own bits are kept: `0o500` for a
/// program only this user may run.
pub fn replace_private_as(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path with no file name"))?;
    let partial = path.with_file_name(format!(
        ".{}.{}.{}.partial",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode & 0o700)
            .open(&partial)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&partial, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&partial);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn the_file_is_this_users_alone_and_replaces_the_old_one_whole() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("state.json");
        replace_private(&path, b"first, and longer than the second").expect("write");
        replace_private(&path, b"second").expect("write again");
        assert_eq!(std::fs::read(&path).expect("read"), b"second");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("state.json")]);
    }

    #[test]
    fn a_mode_keeps_only_the_users_own_bits() {
        let dir = tempfile::tempdir().expect("dir");
        let mode_of = |name: &str, mode: u32| {
            let path = dir.path().join(name);
            replace_private_as(&path, b"x", mode).expect("write");
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777
        };
        assert_eq!(mode_of("program", 0o500), 0o500);
        assert_eq!(mode_of("shared", 0o755), 0o700);
        assert_eq!(mode_of("everyone", 0o777), 0o700);
    }

    #[test]
    fn a_partial_file_left_by_a_crash_does_not_keep_old_permissions() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("state.json");
        std::fs::write(path.with_extension("partial"), b"left behind, world readable").expect("seed");
        std::fs::set_permissions(
            path.with_extension("partial"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("chmod");
        replace_private(&path, b"fresh").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"fresh");
        let mode = std::fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "a file that held a secret must not stay readable by others"
        );
    }

    #[test]
    fn writers_at_once_each_put_a_whole_file_in_place_and_leave_nothing_beside_it() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("hold.json");
        let writes: Vec<Vec<u8>> = (0..16u8).map(|n| vec![n; 4096]).collect();
        std::thread::scope(|scope| {
            for bytes in &writes {
                let path = &path;
                scope.spawn(move || {
                    for _ in 0..20 {
                        replace_private(path, bytes).expect("each write succeeds");
                    }
                });
            }
        });
        let read = std::fs::read(&path).expect("read");
        assert!(writes.contains(&read), "a whole write, not a mix");
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .expect("list")
            .map(|e| e.expect("entry").file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("hold.json")]);
    }

    #[test]
    fn a_path_with_no_file_name_is_refused() {
        assert_eq!(
            replace_private(Path::new("/"), b"x").map_err(|e| e.kind()),
            Err(std::io::ErrorKind::InvalidInput)
        );
    }
}

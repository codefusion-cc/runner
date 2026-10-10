//! Files that hold a secret or state that only this user may read.

use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::Path;

/// Writes a file for this user alone and puts it in place in one step, so that a reader never sees half of it.
///
/// The bytes go to `<path with extension partial>` (mode 0600, synced) and that file is renamed over `path`. A crash
/// leaves the old file whole, or the new one, and at worst a `.partial` file beside it. The folder must exist.
pub fn replace_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let partial = path.with_extension("partial");
    // A file a crash left here keeps its permissions when opened again, so it goes first.
    match std::fs::remove_file(&partial) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    std::fs::rename(&partial, path)
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
}

//! The Time Machine exclusion: what is decided for every state an attribute can be in, and, on a Mac, that the mark
//! is the one `tmutil` makes and reads.

use cf_host_macos::{ATTRIBUTE, Attributes, Exclusion, VALUE, exclude_with};
use std::cell::RefCell;
use std::io;
use std::path::Path;

/// One file's attribute and what was asked of it.
#[derive(Default)]
struct Scripted {
    value: RefCell<Option<Vec<u8>>>,
    reads: RefCell<Vec<String>>,
    writes: RefCell<Vec<(String, Vec<u8>)>>,
    fail_get: bool,
    fail_set: bool,
}

impl Scripted {
    fn with(value: Option<&[u8]>) -> Scripted {
        Scripted {
            value: RefCell::new(value.map(<[u8]>::to_vec)),
            ..Scripted::default()
        }
    }
}

impl Attributes for Scripted {
    fn get(&self, _path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        self.reads.borrow_mut().push(name.into());
        if self.fail_get {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        Ok(self.value.borrow().clone())
    }

    fn set(&self, _path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
        self.writes.borrow_mut().push((name.into(), value.to_vec()));
        if self.fail_set {
            return Err(io::Error::from(io::ErrorKind::Unsupported));
        }
        *self.value.borrow_mut() = Some(value.to_vec());
        Ok(())
    }
}

#[test]
fn a_folder_without_the_mark_gets_it() {
    let folder = Scripted::with(None);
    assert_eq!(
        exclude_with(&folder, Path::new("/state")).unwrap(),
        Exclusion::Marked
    );
    assert_eq!(
        *folder.writes.borrow(),
        vec![(ATTRIBUTE.to_string(), VALUE.to_vec())]
    );
}

#[test]
fn the_mark_is_set_once_and_asking_again_writes_nothing() {
    let folder = Scripted::with(None);
    exclude_with(&folder, Path::new("/state")).unwrap();
    assert_eq!(
        exclude_with(&folder, Path::new("/state")).unwrap(),
        Exclusion::AlreadyMarked
    );
    assert_eq!(
        exclude_with(&folder, Path::new("/state")).unwrap(),
        Exclusion::AlreadyMarked
    );
    assert_eq!(folder.writes.borrow().len(), 1);
}

#[test]
fn another_value_under_the_name_is_replaced_by_the_mark() {
    for other in [&b""[..], b"x", &VALUE[..60], b"bplist00"] {
        let folder = Scripted::with(Some(other));
        assert_eq!(
            exclude_with(&folder, Path::new("/state")).unwrap(),
            Exclusion::Marked,
            "{other:?}"
        );
        assert_eq!(folder.value.borrow().as_deref(), Some(&VALUE[..]));
    }
}

#[test]
fn a_mark_that_cannot_be_read_is_an_error_and_nothing_is_written() {
    let folder = Scripted {
        fail_get: true,
        ..Scripted::default()
    };
    assert_eq!(
        exclude_with(&folder, Path::new("/state")).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(folder.writes.borrow().is_empty());
}

#[test]
fn a_mark_that_cannot_be_set_is_an_error() {
    let folder = Scripted {
        fail_set: true,
        ..Scripted::default()
    };
    assert_eq!(
        exclude_with(&folder, Path::new("/state")).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

#[test]
fn the_value_is_a_property_list_of_the_string_time_machine_expects() {
    assert_eq!(&VALUE[..8], b"bplist00");
    assert_eq!(&VALUE[11..28], b"com.apple.backupd");
    // The string is at offset 8; the trailer says the table of offsets starts after it, at 28.
    assert_eq!(VALUE[28], 8);
    assert_eq!(VALUE[VALUE.len() - 1], 28);
}

#[cfg(target_os = "macos")]
mod on_this_mac {
    use super::*;
    use cf_host_macos::exclude_from_backup;
    use std::process::Command;

    fn tmutil(arguments: &[&str], path: &Path) -> String {
        let out = Command::new("/usr/bin/tmutil")
            .args(arguments)
            .arg(path)
            .output()
            .expect("tmutil runs");
        assert!(
            out.status.success(),
            "tmutil {arguments:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn mark_of(path: &Path) -> String {
        let out = Command::new("/usr/bin/xattr")
            .args(["-px", ATTRIBUTE])
            .arg(path)
            .output()
            .expect("xattr runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    #[test]
    fn time_machine_reads_the_folder_as_excluded_and_a_second_call_changes_nothing() {
        // The temporary folder is excluded by macOS itself, so it would say nothing: the home folder is not.
        let home = std::env::var("HOME").expect("HOME");
        let scratch = tempfile::tempdir_in(home).expect("folder");
        let (ours, included) = (scratch.path().join("ours"), scratch.path().join("included"));
        std::fs::create_dir(&ours).unwrap();
        std::fs::create_dir(&included).unwrap();
        assert!(tmutil(&["isexcluded"], &ours).contains("[Included]"));

        assert_eq!(exclude_from_backup(&ours).unwrap(), Exclusion::Marked);
        assert!(tmutil(&["isexcluded"], &ours).contains("[Excluded]"));
        assert_eq!(exclude_from_backup(&ours).unwrap(), Exclusion::AlreadyMarked);
        assert!(tmutil(&["isexcluded"], &ours).contains("[Excluded]"));
        assert!(tmutil(&["isexcluded"], &included).contains("[Included]"));
    }

    #[test]
    fn the_mark_is_byte_for_byte_the_one_tmutil_makes() {
        let scratch = tempfile::tempdir().expect("folder");
        let (ours, theirs) = (scratch.path().join("ours"), scratch.path().join("theirs"));
        std::fs::create_dir(&ours).unwrap();
        std::fs::create_dir(&theirs).unwrap();
        tmutil(&["addexclusion"], &theirs);
        exclude_from_backup(&ours).unwrap();
        let wanted = mark_of(&theirs);
        assert!(
            wanted.starts_with("62 70 6C 69 73 74 30 30"),
            "tmutil's mark: {wanted}"
        );
        assert_eq!(mark_of(&ours), wanted);
    }

    #[test]
    fn a_folder_that_is_not_there_is_an_error_and_not_a_panic() {
        let scratch = tempfile::tempdir().expect("folder");
        let error = exclude_from_backup(&scratch.path().join("gone")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn a_name_with_a_nul_is_an_error() {
        use std::os::unix::ffi::OsStrExt as _;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/tmp/a\0b"));
        assert_eq!(
            exclude_from_backup(path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

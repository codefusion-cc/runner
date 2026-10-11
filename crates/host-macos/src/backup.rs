//! Keeping the Runner's folders out of Time Machine.
//!
//! A guest image is gigabytes, a scratch disk is rewritten by every job, and none of it is worth a backup: the image
//! comes from a release again and a scratch disk is formatted anew. The Runner marks the folders it makes with the
//! sticky exclusion `tmutil addexclusion <path>` sets: the extended attribute [`ATTRIBUTE`]. Time Machine skips the
//! folder and everything below it, and the mark stays with the folder, not with a path in a list.
//!
//! A folder the Runner will make later for caches goes through [`exclude_from_backup`] when it is made.

use std::io;
use std::path::Path;

/// The extended attribute that tells Time Machine to skip a file or folder (what `tmutil addexclusion` sets).
pub const ATTRIBUTE: &str = "com.apple.metadata:com_apple_backup_excludeItem";

/// The attribute's value: the binary property list `tmutil addexclusion` writes, the string `com.apple.backupd`.
pub const VALUE: [u8; 61] = [
    0x62, 0x70, 0x6C, 0x69, 0x73, 0x74, 0x30, 0x30, // "bplist00"
    0x5F, 0x10, 0x11, // an ASCII string of 17 bytes
    0x63, 0x6F, 0x6D, 0x2E, 0x61, 0x70, 0x70, 0x6C, 0x65, 0x2E, 0x62, 0x61, 0x63, 0x6B, 0x75, 0x70, 0x64,
    0x08, // the offset table: the string is at 8
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    0x01, // the trailer: sizes of an offset and of a reference
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, // one object
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // the top object is the first
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1C, // the offset table is at 28
];

/// What [`exclude_from_backup`] found and did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exclusion {
    /// The folder carried no mark, or another value, and now carries the mark.
    Marked,
    /// The folder carried the mark already and was left as it is.
    AlreadyMarked,
}

/// Extended attributes of a file: the edge [`exclude_with`] decides over, so the decision is tested without a disk.
pub trait Attributes {
    /// The value of the attribute `name` on `path`; `None` when the file has no such attribute.
    fn get(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>>;
    /// Sets the attribute `name` on `path` to `value`, replacing one that is there.
    fn set(&self, path: &Path, name: &str, value: &[u8]) -> io::Result<()>;
}

/// Marks `path` as excluded from Time Machine through `attributes`, and writes nothing when the mark is there.
///
/// An error is a mark that could not be read or set (a file system without extended attributes, a folder that is
/// not this user's). The caller says so and goes on: a backup that includes a folder is a waste, not a failure.
pub fn exclude_with(attributes: &dyn Attributes, path: &Path) -> io::Result<Exclusion> {
    if attributes.get(path, ATTRIBUTE)?.as_deref() == Some(&VALUE[..]) {
        return Ok(Exclusion::AlreadyMarked);
    }
    attributes.set(path, ATTRIBUTE, &VALUE)?;
    Ok(Exclusion::Marked)
}

/// Marks `path` (a folder that exists) as excluded from Time Machine, the way `tmutil addexclusion` does. Setting it
/// again changes nothing. An error is a mark that could not be set; the caller logs it and goes on.
#[cfg(target_os = "macos")]
pub fn exclude_from_backup(path: &Path) -> io::Result<Exclusion> {
    exclude_with(&Xattr, path)
}

/// The extended attributes of this Mac's file system.
#[cfg(target_os = "macos")]
struct Xattr;

#[cfg(target_os = "macos")]
fn c_string(bytes: &[u8]) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a NUL in a path or name"))
}

#[cfg(target_os = "macos")]
impl Attributes for Xattr {
    fn get(&self, path: &Path, name: &str) -> io::Result<Option<Vec<u8>>> {
        use std::os::unix::ffi::OsStrExt as _;
        let (path, name) = (c_string(path.as_os_str().as_bytes())?, c_string(name.as_bytes())?);
        // The value can be anything another program put there; one that does not fit in the buffer is not ours.
        let mut buffer = [0u8; 256];
        // SAFETY: `path` and `name` are NUL-terminated and outlive the call; the buffer is writable for the
        // 256 bytes given as its size.
        let read = unsafe {
            libc::getxattr(
                path.as_ptr(),
                name.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                0,
                0,
            )
        };
        if read >= 0 {
            return Ok(Some(buffer[..read as usize].to_vec()));
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::ENOATTR) => Ok(None),
            // The value is longer than the buffer, so it is not our mark.
            Some(libc::ERANGE) => Ok(Some(Vec::new())),
            _ => Err(error),
        }
    }

    fn set(&self, path: &Path, name: &str, value: &[u8]) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let (path, name) = (c_string(path.as_os_str().as_bytes())?, c_string(name.as_bytes())?);
        // SAFETY: `path` and `name` are NUL-terminated and outlive the call; `value` is readable for its length.
        let done = unsafe {
            libc::setxattr(
                path.as_ptr(),
                name.as_ptr(),
                value.as_ptr().cast(),
                value.len(),
                0,
                0,
            )
        };
        if done == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

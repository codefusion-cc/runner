//! A job's cache disk in the guest: mounted at the job's start, its folders ready for the job's user, and at the
//! job's end left with packages only and let go. What goes where is in [`cf_supervisor::cache`].

use super::job::{disk, image_tool, mke2fs_args};
use super::sys::{self, context, say};
use cf_guest_proto::{CACHE_DISK, CacheFound};
use cf_supervisor::cache::{self as plan, MOUNT};
use std::fs::{self, File};
use std::io::Read;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt};
use std::path::Path;
use std::time::Duration;

/// How long the cache disk may stay busy once every process of the job is gone.
const LET_GO: Duration = Duration::from_secs(5);

/// Mounts the cache disk at [`MOUNT`] and says what it held. A disk the image's `e2fsck` cannot make whole, or that
/// does not mount, is made anew: the job runs with an empty cache rather than none.
pub fn mount() -> Result<CacheFound, String> {
    let device = disk(CACHE_DISK)?;
    let flags = libc::MS_NOATIME | libc::MS_NODEV | libc::MS_NOSUID;
    context("the cache's folder", fs::create_dir_all(MOUNT))?;
    let empty = empty_disk(&device);
    if !empty {
        // `-p` repairs what it safely can and says 4 or more for what it cannot.
        let checked = image_tool(&["e2fsck".into(), "-p".into(), device.clone()])
            .is_ok_and(|status| status.code().is_some_and(|code| code < 4));
        if checked && sys::mount(&device, MOUNT, "ext4", flags, "").is_ok() {
            return Ok(CacheFound::Reused);
        }
        say("the cache disk does not mount: it is made anew");
    }
    let found = if empty {
        CacheFound::Fresh
    } else {
        CacheFound::Replaced
    };
    let status = image_tool(&mke2fs_args(&device, true, empty))?;
    if !status.success() {
        return Err(format!("the image's mke2fs ended with {status}"));
    }
    sys::mount_at(&device, MOUNT, "ext4", flags, "")?;
    Ok(found)
}

/// Whether a disk's first megabyte is all zeros: a file the host made empty, never formatted.
fn empty_disk(device: &str) -> bool {
    let mut start = vec![0u8; 1 << 20];
    File::open(device)
        .and_then(|mut file| file.read_exact(&mut start))
        .is_ok_and(|()| start.iter().all(|byte| *byte == 0))
}

/// The folders the job's user writes, made if new and given to the user (`uid`, `gid`) alone. A folder another user
/// owns (an earlier image's) is given over with everything in it; one already the user's is left as it is, so a
/// store of many files costs nothing at the start. The top level stays root's.
pub fn ready(uid: u32, gid: u32) -> Result<(), String> {
    context(
        "the cache's top level",
        fs::set_permissions(MOUNT, fs::Permissions::from_mode(0o755)),
    )?;
    // A link or a file where a folder goes is no folder: it goes, and the guest's root never follows it.
    for folder in plan::KEPT {
        let path = format!("{MOUNT}/{folder}");
        if fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_dir()) {
            context(&path, fs::remove_file(&path))?;
        }
    }
    for folder in plan::USER_FOLDERS {
        let path = format!("{MOUNT}/{folder}");
        context(&path, fs::create_dir_all(&path))?;
        let meta = context(&path, fs::symlink_metadata(&path))?;
        if (meta.uid(), meta.gid()) != (uid, gid) {
            give(Path::new(&path), uid, gid)?;
        }
        context(
            &path,
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)),
        )?;
    }
    Ok(())
}

/// Gives `path` and what it holds to `uid` and `gid`, without following links: only what someone else owns is
/// changed, so a walk over a folder that is already the user's only reads.
fn give(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let shown = || path.display().to_string();
    let meta = context(&shown(), fs::symlink_metadata(path))?;
    if (meta.uid(), meta.gid()) != (uid, gid) {
        context(&shown(), std::os::unix::fs::lchown(path, Some(uid), Some(gid)))?;
    }
    if meta.is_dir() {
        for entry in context(&shown(), fs::read_dir(path))?.flatten() {
            give(&entry.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// The entries of a folder of the cache, each with whether it is a folder (not a link to one); none when it cannot
/// be read or is no folder itself.
fn entries(folder: &Path) -> Vec<plan::Entry> {
    if !fs::symlink_metadata(folder).is_ok_and(|meta| meta.is_dir()) {
        return Vec::new();
    }
    fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| {
            let folder = entry.file_type().is_ok_and(|kind| kind.is_dir());
            (entry.file_name().to_string_lossy().into_owned(), folder)
        })
        .collect()
}

/// Once every process of the job is gone: deletes what is not a package ([`plan::leftovers`]), gives what the job's
/// root left in the user's folders (`sudo npm ci`) back to the user (`uid`, `gid`), then unmounts the cache, which a
/// Docker job's sandbox held until its namespaces went. Says whether the disk was let go cleanly; one still mounted,
/// or with leftovers that would not go, is not kept.
pub fn let_go(uid: u32, gid: u32) -> bool {
    let mut whole = true;
    let mount = Path::new(MOUNT);
    for gone in plan::leftovers(&entries(mount), &entries(&mount.join(plan::NPM))) {
        let path = format!("{MOUNT}/{gone}");
        let removed = match fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(&path),
            Ok(_) => fs::remove_file(&path),
            Err(error) => Err(error),
        };
        if let Err(error) = removed {
            say(&format!("{path} stays on the cache disk: {error}"));
            whole = false;
        }
    }
    for folder in plan::USER_FOLDERS {
        if let Err(error) = give(&Path::new(MOUNT).join(folder), uid, gid) {
            say(&format!(
                "the cache's {folder} folder is not all the job user's: {error}"
            ));
            whole = false;
        }
    }
    let mut busy = None;
    let let_go = sys::until(LET_GO, Duration::from_millis(100), || {
        sys::umount(MOUNT, 0).map_err(|error| busy = Some(error)).ok()
    });
    if let (None, Some(error)) = (let_go, busy) {
        say(&format!("the cache disk stays busy: {error}"));
    }
    whole && let_go.is_some()
}

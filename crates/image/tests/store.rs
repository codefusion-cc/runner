//! The image store against what an installed image must be and the releases it must refuse.

use cf_image::store::{ROOTFS_NAME, Store, StoreError};
use cf_image::{ImageError, ReleaseInputs, write_release};
use ed25519_dalek::SigningKey;
use std::fs;
use std::path::{Path, PathBuf};

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn rootfs() -> Vec<u8> {
    (0..2500u32).flat_map(|n| n.to_le_bytes()).collect()
}

/// A release folder of this version and build time, its root file system in parts of 4096 bytes.
fn release(version: &str, built: u64, with_rootfs: bool, signer: &SigningKey) -> tempfile::TempDir {
    let src = tempfile::tempdir().unwrap();
    let out = tempfile::tempdir().unwrap();
    fs::write(src.path().join("Image"), b"kernel").unwrap();
    fs::write(src.path().join("initramfs"), b"initramfs").unwrap();
    fs::write(src.path().join("rootfs"), rootfs()).unwrap();
    let rootfs = src.path().join("rootfs");
    write_release(
        out.path(),
        &ReleaseInputs {
            version,
            built,
            kernel: &src.path().join("Image"),
            initramfs: &src.path().join("initramfs"),
            rootfs: with_rootfs.then_some(rootfs.as_path()),
            max_part: 4096,
        },
        signer,
    )
    .unwrap();
    out
}

fn part(release: &Path) -> PathBuf {
    let mut parts: Vec<PathBuf> = fs::read_dir(release)
        .unwrap()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.to_string_lossy().contains("rootfs"))
        .collect();
    parts.sort();
    assert!(
        parts.len() >= 3,
        "the test's release has several parts: {parts:?}"
    );
    parts[1].clone()
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn an_installed_image_is_the_signed_root_file_system_in_one_file_only_the_owner_reads() {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    assert_eq!((store.rootfs("v1"), store.newest_accepted()), (None, 0));
    let good = release("v1", 1000, true, &key(1));
    let manifest = store.install(good.path(), "v1", &key(1).verifying_key()).unwrap();
    assert_eq!((manifest.version.as_str(), manifest.built), ("v1", 1000));
    let installed = store.rootfs("v1").unwrap();
    assert_eq!(installed, state.path().join("images/v1").join(ROOTFS_NAME));
    assert_eq!(fs::read(&installed).unwrap(), rootfs());
    assert_eq!(store.newest_accepted(), 1000);
    assert!(
        !names(&state.path().join("images/v1"))
            .iter()
            .any(|name| name.contains("erofs")),
        "the parts are joined, not kept twice"
    );
    assert_eq!(names(&state.path().join("images")), ["accepted.json", "v1"]);
    assert_eq!(
        fs::metadata(state.path().join("images/v1"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    // The source can change afterwards; what is installed does not follow it.
    fs::write(part(good.path()), b"changed").unwrap();
    assert_eq!(fs::read(&installed).unwrap(), rootfs());
}

#[test]
fn a_release_with_a_changed_part_another_key_or_another_version_installs_nothing() {
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    let trusted = key(1).verifying_key();

    let changed = release("v1", 1000, true, &key(1));
    let path = part(changed.path());
    let mut bytes = fs::read(&path).unwrap();
    bytes[7] ^= 1;
    fs::write(&path, bytes).unwrap();
    assert!(matches!(
        store.install(changed.path(), "v1", &trusted),
        Err(StoreError::Refused(ImageError::HashMismatch(_)))
    ));

    let forged = release("v1", 1000, true, &key(2));
    assert!(matches!(
        store.install(forged.path(), "v1", &trusted),
        Err(StoreError::Refused(ImageError::BadSignature))
    ));

    let other = release("v2", 1000, true, &key(1));
    assert!(matches!(
        store.install(other.path(), "v1", &trusted),
        Err(StoreError::Refused(_))
    ));

    let bundle = release("v1", 1000, false, &key(1));
    assert!(matches!(
        store.install(bundle.path(), "v1", &trusted),
        Err(StoreError::NoRootfs)
    ));

    assert!(matches!(
        store.install(&state.path().join("nowhere"), "v1", &trusted),
        Err(StoreError::Io(..))
    ));
    assert_eq!(names(&state.path().join("images")), Vec::<String>::new());
    assert_eq!((store.rootfs("v1"), store.newest_accepted()), (None, 0));
}

#[test]
fn an_older_release_is_refused_once_a_newer_one_was_accepted() {
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    let trusted = key(1).verifying_key();
    store
        .install(release("v2", 2000, true, &key(1)).path(), "v2", &trusted)
        .unwrap();
    let old = release("v1", 1999, true, &key(1));
    assert!(matches!(
        store.install(old.path(), "v1", &trusted),
        Err(StoreError::Refused(ImageError::Older {
            built: 1999,
            newest_accepted: 2000
        }))
    ));
    assert_eq!(store.rootfs("v1"), None);
    // The same release again, and a newer one, are fine; the newest build time only moves forward.
    store
        .install(release("v2", 2000, true, &key(1)).path(), "v2", &trusted)
        .unwrap();
    store
        .install(release("v3", 2001, true, &key(1)).path(), "v3", &trusted)
        .unwrap();
    store
        .install(release("v2", 2001, true, &key(1)).path(), "v2", &trusted)
        .unwrap();
    assert_eq!(store.newest_accepted(), 2001);
    // A store built anew on the same folder remembers.
    assert_eq!(Store::new(state.path().join("images")).newest_accepted(), 2001);
}

#[test]
fn a_version_that_is_a_path_names_no_folder() {
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    let good = release("v1", 1000, true, &key(1));
    for version in [
        "../v1",
        "a/b",
        "",
        ".",
        "..",
        ".hidden",
        "v 1",
        "v1\n",
        &"v".repeat(65),
    ] {
        assert!(
            matches!(
                store.install(good.path(), version, &key(1).verifying_key()),
                Err(StoreError::Version(_))
            ),
            "{version:?}"
        );
        assert_eq!(store.rootfs(version), None);
    }
    assert!(!state.path().join("images").exists() && names(state.path()).is_empty());
}

#[test]
fn a_link_in_the_release_folder_is_not_followed() {
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    let good = release("v1", 1000, true, &key(1));
    // The part is replaced by a link to a file with the very same bytes, which could change after the check.
    let path = part(good.path());
    let elsewhere = good.path().join("elsewhere.bin");
    fs::rename(&path, &elsewhere).unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::rename(&elsewhere, outside.path().join("part")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("part"), &path).unwrap();
    assert!(matches!(
        store.install(good.path(), "v1", &key(1).verifying_key()),
        Err(StoreError::Refused(ImageError::Missing(_)))
    ));
    assert_eq!(store.rootfs("v1"), None);
}

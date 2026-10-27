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
    release_for(cf_image::HOST_ARCH, version, built, with_rootfs, signer)
}

/// The same, built for `arch`.
fn release_for(
    arch: &str,
    version: &str,
    built: u64,
    with_rootfs: bool,
    signer: &SigningKey,
) -> tempfile::TempDir {
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
            arch,
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
    assert_eq!((store.rootfs("v1"), store.newest_accepted().unwrap()), (None, 0));
    let good = release("v1", 1000, true, &key(1));
    let manifest = store.install(good.path(), "v1", &key(1).verifying_key()).unwrap();
    assert_eq!((manifest.version.as_str(), manifest.built), ("v1", 1000));
    let installed = store.rootfs("v1").unwrap();
    assert_eq!(installed, state.path().join("images/v1").join(ROOTFS_NAME));
    assert_eq!(fs::read(&installed).unwrap(), rootfs());
    assert_eq!(store.newest_accepted().unwrap(), 1000);
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
    assert_eq!((store.rootfs("v1"), store.newest_accepted().unwrap()), (None, 0));
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
    assert_eq!(store.newest_accepted().unwrap(), 2001);
    // A store built anew on the same folder remembers.
    assert_eq!(
        Store::new(state.path().join("images")).newest_accepted().unwrap(),
        2001
    );
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

#[test]
fn a_record_of_the_newest_release_that_cannot_be_read_refuses_every_release() {
    let state = tempfile::tempdir().unwrap();
    let images = state.path().join("images");
    let store = Store::new(&images);
    let trusted = key(1).verifying_key();
    store
        .install(release("v2", 2000, true, &key(1)).path(), "v2", &trusted)
        .unwrap();
    let record = images.join("accepted.json");
    // Read as "nothing accepted yet", each of these would let the release of 1999 back in.
    for damaged in [
        &b"two thousand"[..],
        b"",
        b"-1",
        b"2000 1",
        b"\xff\xfe",
        b"18446744073709551616",
    ] {
        fs::write(&record, damaged).unwrap();
        let old = release("v1", 1999, true, &key(1));
        let error = store.install(old.path(), "v1", &trusted).unwrap_err();
        assert!(matches!(error, StoreError::Accepted(..)), "{damaged:?}: {error}");
        assert_eq!(store.rootfs("v1"), None, "{damaged:?}");
        assert!(store.newest_accepted().is_err());
        assert_eq!(names(&images), ["accepted.json", "v2"]);
    }
    fs::remove_file(&record).unwrap();
    fs::create_dir(&record).unwrap();
    assert!(matches!(
        store.install(release("v1", 1999, true, &key(1)).path(), "v1", &trusted),
        Err(StoreError::Accepted(..))
    ));
    // A number with the line's end an editor leaves is a number.
    fs::remove_dir(&record).unwrap();
    fs::write(&record, "2000\n").unwrap();
    assert_eq!(store.newest_accepted().unwrap(), 2000);
    assert!(matches!(
        store.install(release("v1", 1999, true, &key(1)).path(), "v1", &trusted),
        Err(StoreError::Refused(ImageError::Older { .. }))
    ));
}

#[test]
fn a_version_installed_again_takes_the_firsts_place_and_leaves_no_work_folder() {
    let state = tempfile::tempdir().unwrap();
    let images = state.path().join("images");
    let store = Store::new(&images);
    let trusted = key(1).verifying_key();
    // Versions that end as the store's own work folders once did are versions like any other.
    for version in ["v1", "v1.old", "v1.partial"] {
        store
            .install(release(version, 1000, true, &key(1)).path(), version, &trusted)
            .unwrap();
    }
    fs::write(images.join("v1").join("mark"), b"the first install").unwrap();
    store
        .install(release("v1", 1001, true, &key(1)).path(), "v1", &trusted)
        .unwrap();
    assert_eq!(names(&images), ["accepted.json", "v1", "v1.old", "v1.partial"]);
    assert_eq!(
        names(&images.join("v1")),
        [
            "Image",
            "SHA256SUMS",
            "SHA256SUMS.sig",
            "image.json",
            "initramfs",
            "rootfs.img"
        ]
    );
    for version in ["v1", "v1.old", "v1.partial"] {
        assert_eq!(
            fs::read(store.rootfs(version).unwrap()).unwrap(),
            rootfs(),
            "{version}"
        );
    }
    assert_eq!(store.newest_accepted().unwrap(), 1001);
}

#[test]
fn a_release_whose_build_time_cannot_be_recorded_is_not_installed() {
    let state = tempfile::tempdir().unwrap();
    let images = state.path().join("images");
    let store = Store::new(&images);
    let trusted = key(1).verifying_key();
    store
        .install(release("v1", 1000, true, &key(1)).path(), "v1", &trusted)
        .unwrap();
    // The record's next version cannot be written: something else is where it goes.
    fs::create_dir(images.join("accepted.json.new")).unwrap();
    let error = store
        .install(release("v2", 2000, true, &key(1)).path(), "v2", &trusted)
        .unwrap_err();
    assert!(matches!(error, StoreError::Io(..)), "{error}");
    // Installed without its record, v2 could later be followed by a release older than itself.
    assert_eq!(store.rootfs("v2"), None);
    assert_eq!(store.newest_accepted().unwrap(), 1000);
    assert_eq!(names(&images), ["accepted.json", "accepted.json.new", "v1"]);
}

#[test]
fn what_the_signed_list_does_not_name_is_not_installed() {
    use std::os::unix::fs::PermissionsExt;
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    let good = release("v1", 1000, true, &key(1));
    // A genuine release with files next to it that nobody signed: one under the installed image's own name.
    fs::write(good.path().join(ROOTFS_NAME), b"not the signed root file system").unwrap();
    fs::write(good.path().join("run-me.sh"), b"#!/bin/sh\n").unwrap();
    fs::write(good.path().join(".hidden"), b"x").unwrap();
    store.install(good.path(), "v1", &key(1).verifying_key()).unwrap();

    let installed = state.path().join("images/v1");
    assert_eq!(fs::read(installed.join(ROOTFS_NAME)).unwrap(), rootfs());
    assert_eq!(
        names(&installed),
        [
            "Image",
            "SHA256SUMS",
            "SHA256SUMS.sig",
            "image.json",
            "initramfs",
            ROOTFS_NAME
        ]
    );
    assert_eq!(
        fs::metadata(installed.join(ROOTFS_NAME))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn a_second_install_waits_for_the_one_under_way() {
    let state = tempfile::tempdir().unwrap();
    let images = state.path().join("images");
    let store = Store::new(&images);
    let good = release("v1", 1000, true, &key(1));
    store.install(good.path(), "v1", &key(1).verifying_key()).unwrap();

    // An install under way holds the store, as this test does now.
    let held = fs::File::open(&images).unwrap();
    held.lock().unwrap();
    let (said, heard) = std::sync::mpsc::channel();
    let source = good.path().to_path_buf();
    let second = std::thread::spawn(move || {
        let outcome = Store::new(&images).install(&source, "v1", &key(1).verifying_key());
        said.send(()).unwrap();
        outcome
    });
    assert!(
        heard.recv_timeout(std::time::Duration::from_millis(500)).is_err(),
        "the second install went on while the first held the store"
    );
    assert_eq!(names(&state.path().join("images")), ["accepted.json", "v1"]);
    held.unlock().unwrap();
    heard.recv_timeout(std::time::Duration::from_secs(20)).unwrap();
    assert_eq!(second.join().unwrap().unwrap().version, "v1");
    assert_eq!(names(&state.path().join("images")), ["accepted.json", "v1"]);
}

#[test]
fn an_image_of_another_architecture_is_refused_and_nothing_is_installed() {
    let state = tempfile::tempdir().unwrap();
    let store = Store::new(state.path().join("images"));
    let trusted = key(1).verifying_key();
    let other = if cf_image::HOST_ARCH == "x86_64" {
        "aarch64"
    } else {
        "x86_64"
    };
    for arch in [other, "", "arm64", "amd64"] {
        let foreign = release_for(arch, "v1", 1000, true, &key(1));
        match store.install(foreign.path(), "v1", &trusted) {
            Err(StoreError::Refused(ImageError::WrongArch { found, host })) => {
                assert_eq!((found.as_str(), host.as_str()), (arch, cf_image::HOST_ARCH));
            }
            other => panic!("{arch}: {other:?}"),
        }
    }
    assert_eq!(names(&state.path().join("images")), Vec::<String>::new());
    // A refused image moves nothing forward: the host's own installs after it, older or not.
    assert_eq!((store.rootfs("v1"), store.newest_accepted().unwrap()), (None, 0));
    let own = release("v1", 900, true, &key(1));
    store.install(own.path(), "v1", &trusted).unwrap();
    assert!(store.rootfs("v1").is_some());
}

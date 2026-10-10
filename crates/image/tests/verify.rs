//! The ways a downloaded image can be wrong, each of which must be refused with its own error.

use std::fs;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_image::{
    DirFiles, ImageError, MANIFEST_NAME, MemFiles, ReleaseInputs, SIGNATURE_NAME, SUMS_NAME,
    parse_signing_key, public_key_text, verify, write_release,
};
use ed25519_dalek::{Signer, SigningKey};
use proptest::prelude::*;
use sha2::{Digest, Sha256};

const VERSION: &str = "2026.10.10-1";
/// When the test release says it was built.
const BUILT: u64 = 1_800_000_000;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

struct Release {
    files: MemFiles,
    key: SigningKey,
}

fn release_with(kernel: &[u8], initramfs: &[u8], rootfs: Option<&[u8]>, max_part: u64) -> Release {
    let dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("Image"), kernel).unwrap();
    fs::write(src.path().join("initramfs.cpio.gz"), initramfs).unwrap();
    if let Some(rootfs) = rootfs {
        fs::write(src.path().join("rootfs.erofs"), rootfs).unwrap();
    }
    let key = key(7);
    write_release(
        dir.path(),
        &ReleaseInputs {
            version: VERSION,
            built: BUILT,
            kernel: &src.path().join("Image"),
            initramfs: &src.path().join("initramfs.cpio.gz"),
            rootfs: rootfs.map(|_| src.path().join("rootfs.erofs")).as_deref(),
            max_part,
        },
        &key,
    )
    .unwrap();
    let mut files = MemFiles::new();
    for entry in fs::read_dir(dir.path()).unwrap() {
        let entry = entry.unwrap();
        files.insert(
            entry.file_name().to_str().unwrap(),
            fs::read(entry.path()).unwrap(),
        );
    }
    Release { files, key }
}

fn good() -> Release {
    release_with(&[1; 5000], &[2; 3000], Some(&[3; 2500]), 1000)
}

fn check(release: &Release) -> Result<(), ImageError> {
    verify(&release.files, &release.key.verifying_key(), VERSION, 0).map(|_| ())
}

/// Replaces `SHA256SUMS` and re-signs it, as someone holding the key would (to reach the checks after the signature).
fn resign(release: &mut Release, sums: impl AsRef<[u8]>) {
    let sums = sums.as_ref();
    let signature = release.key.sign(sums);
    release.files.insert(SUMS_NAME, sums);
    release
        .files
        .insert(SIGNATURE_NAME, URL_SAFE_NO_PAD.encode(signature.to_bytes()));
}

/// A file of the release as text.
fn text_of(release: &Release, name: &str) -> String {
    String::from_utf8(release.files.get(name).unwrap().to_vec()).unwrap()
}

fn flip(release: &mut Release, name: &str, at: usize) {
    release.files.get_mut(name).unwrap()[at] ^= 1;
}

#[test]
fn a_good_release_verifies_and_the_parts_cover_the_rootfs() {
    let release = good();
    let verified = verify(&release.files, &release.key.verifying_key(), VERSION, 0).unwrap();
    let rootfs = verified.manifest.rootfs.unwrap();
    assert_eq!(rootfs.size, 2500);
    let sizes: Vec<u64> = rootfs.parts.iter().map(|part| part.size).collect();
    assert_eq!(sizes, [1000, 1000, 500]);
    assert_eq!(rootfs.parts[0].name, "rootfs.erofs.000");
}

#[test]
fn a_release_without_a_rootfs_verifies() {
    let release = release_with(b"k", b"i", None, 1000);
    assert!(check(&release).is_ok());
}

#[test]
fn a_changed_byte_in_any_file_is_refused_by_name() {
    for name in [
        "Image",
        "initramfs.cpio.gz",
        "rootfs.erofs.000",
        "rootfs.erofs.002",
    ] {
        for at in [0, 499] {
            let mut release = good();
            flip(&mut release, name, at);
            assert_eq!(
                check(&release),
                Err(ImageError::HashMismatch(name.into())),
                "{name} at {at}"
            );
        }
    }
}

#[test]
fn a_changed_byte_in_the_manifest_is_refused() {
    let mut release = good();
    flip(&mut release, MANIFEST_NAME, 20);
    assert_eq!(
        check(&release),
        Err(ImageError::HashMismatch(MANIFEST_NAME.into()))
    );
}

#[test]
fn a_changed_byte_in_the_list_of_hashes_breaks_the_signature() {
    for at in [0, 70, 200] {
        let mut release = good();
        flip(&mut release, SUMS_NAME, at);
        assert_eq!(check(&release), Err(ImageError::BadSignature), "byte {at}");
    }
}

#[test]
fn a_signature_by_another_key_is_refused() {
    let release = good();
    let other = key(8).verifying_key();
    assert_eq!(
        verify(&release.files, &other, VERSION, 0).map(|_| ()),
        Err(ImageError::BadSignature)
    );
}

#[test]
fn a_signature_that_is_not_a_signature_is_refused() {
    for garbage in ["", "not base64 !!", "AAAA", &"A".repeat(86)] {
        let mut release = good();
        release.files.insert(SIGNATURE_NAME, garbage);
        let error = check(&release).unwrap_err();
        assert!(
            matches!(error, ImageError::SignatureMalformed | ImageError::BadSignature),
            "{garbage:?}: {error:?}"
        );
    }
}

#[test]
fn a_signature_of_another_list_is_refused() {
    let mut release = good();
    let other = release.key.sign(b"something else");
    release
        .files
        .insert(SIGNATURE_NAME, URL_SAFE_NO_PAD.encode(other.to_bytes()));
    assert_eq!(check(&release), Err(ImageError::BadSignature));
}

#[test]
fn a_truncated_or_extended_file_is_refused_with_its_sizes() {
    let mut release = good();
    release.files.get_mut("Image").unwrap().truncate(4999);
    assert_eq!(
        check(&release),
        Err(ImageError::WrongSize {
            name: "Image".into(),
            expected: 5000,
            actual: 4999
        })
    );
    let mut release = good();
    release.files.get_mut("rootfs.erofs.001").unwrap().push(0);
    assert_eq!(
        check(&release),
        Err(ImageError::WrongSize {
            name: "rootfs.erofs.001".into(),
            expected: 1000,
            actual: 1001
        })
    );
    let mut release = good();
    release.files.get_mut("rootfs.erofs.002").unwrap().clear();
    assert_eq!(
        check(&release),
        Err(ImageError::WrongSize {
            name: "rootfs.erofs.002".into(),
            expected: 500,
            actual: 0
        })
    );
}

#[test]
fn a_missing_file_is_refused_by_name() {
    for name in [
        SUMS_NAME,
        SIGNATURE_NAME,
        MANIFEST_NAME,
        "Image",
        "initramfs.cpio.gz",
        "rootfs.erofs.001",
    ] {
        let mut release = good();
        release.files.remove(name);
        assert_eq!(check(&release), Err(ImageError::Missing(name.into())), "{name}");
    }
}

#[test]
fn a_release_for_another_version_is_refused_even_when_it_is_genuine() {
    let release = good();
    let error = verify(&release.files, &release.key.verifying_key(), "2026.10.11-1", 0).unwrap_err();
    assert_eq!(
        error,
        ImageError::WrongVersion {
            expected: "2026.10.11-1".into(),
            found: VERSION.into()
        }
    );
    // The same for a prefix or a case change of the version.
    for asked in ["2026.10.10", "2026.10.10-1 ", "2026.10.10-1\n"] {
        assert!(matches!(
            verify(&release.files, &release.key.verifying_key(), asked, 0),
            Err(ImageError::WrongVersion { .. })
        ));
    }
}

#[test]
fn a_manifest_swapped_in_from_another_release_is_refused() {
    let older = release_with(&[1; 10], &[2; 10], None, 1000);
    let mut release = good();
    release
        .files
        .insert(MANIFEST_NAME, older.files.get(MANIFEST_NAME).unwrap());
    assert_eq!(
        check(&release),
        Err(ImageError::HashMismatch(MANIFEST_NAME.into()))
    );
}

#[test]
fn a_signed_list_that_disagrees_with_the_manifest_is_refused() {
    let mut release = good();
    let sums = text_of(&release, SUMS_NAME);
    let kernel_hash = hex(&Sha256::digest([1u8; 5000]));
    let tampered = sums.replace(&kernel_hash, &"0".repeat(64));
    resign(&mut release, &tampered);
    assert_eq!(
        check(&release),
        Err(ImageError::ManifestDisagrees("Image".into()))
    );

    // A file in the signed list that the manifest does not name.
    let mut release = good();
    let sums = text_of(&release, SUMS_NAME) + &format!("{}  extra.bin\n", "a".repeat(64));
    resign(&mut release, &sums);
    assert_eq!(
        check(&release),
        Err(ImageError::ManifestDisagrees("extra.bin".into()))
    );
}

#[test]
fn a_signed_list_with_hostile_lines_is_refused() {
    let hash = "a".repeat(64);
    let cases = [
        format!("{hash}  ../etc/passwd\n"),
        format!("{hash}  /abs\n"),
        format!("{hash}  a/b\n"),
        format!("{hash}  ..\n"),
        format!("{hash} single-space\n"),
        format!("{}  upper\n", "A".repeat(64)),
        format!("{}  short\n", "a".repeat(63)),
        format!("{hash}  dup\n{hash}  dup\n"),
        format!("{hash}  \n"),
    ];
    for sums in cases {
        let mut release = good();
        resign(&mut release, &sums);
        assert!(
            matches!(check(&release), Err(ImageError::SumsMalformed { .. })),
            "{sums:?}"
        );
    }
}

#[test]
fn a_signed_manifest_that_is_not_a_manifest_is_refused() {
    let mut release = good();
    release.files.insert(MANIFEST_NAME, "{ not json");
    let sums = format!("{}  {MANIFEST_NAME}\n", hex(&Sha256::digest(b"{ not json")));
    resign(&mut release, &sums);
    assert!(matches!(check(&release), Err(ImageError::ManifestMalformed(_))));

    let mut release = good();
    let text = text_of(&release, MANIFEST_NAME);
    let future = text.replace("\"format\": 1", "\"format\": 2");
    release.files.insert(MANIFEST_NAME, future.clone());
    let mut sums = text_of(&release, SUMS_NAME);
    let old = hex(&Sha256::digest(text.as_bytes()));
    sums = sums.replace(&old, &hex(&Sha256::digest(future.as_bytes())));
    resign(&mut release, &sums);
    assert_eq!(
        check(&release),
        Err(ImageError::UnknownFormat {
            found: 2,
            supported: 1
        })
    );
}

#[test]
fn parts_that_do_not_add_up_to_the_whole_are_refused() {
    // Each part is genuine and signed, but the manifest's whole is wrong: it must not pass.
    let mut release = good();
    let text = text_of(&release, MANIFEST_NAME);
    let whole = hex(&Sha256::digest([3u8; 2500]));
    let broken = text.replace(&whole, &"b".repeat(64));
    assert_ne!(text, broken);
    release.files.insert(MANIFEST_NAME, broken.clone());
    let sums = text_of(&release, SUMS_NAME).replace(
        &hex(&Sha256::digest(text.as_bytes())),
        &hex(&Sha256::digest(broken.as_bytes())),
    );
    resign(&mut release, &sums);
    assert_eq!(check(&release), Err(ImageError::RootfsMismatch));
}

#[test]
fn a_list_may_be_one_mebibyte_and_not_a_byte_more() {
    const LIMIT: usize = 1 << 20;
    // One byte over: refused for its size, before the signature or a parser sees it.
    let mut release = good();
    release.files.insert(SUMS_NAME, vec![b'a'; LIMIT + 1]);
    assert_eq!(
        check(&release),
        Err(ImageError::WrongSize {
            name: SUMS_NAME.into(),
            expected: LIMIT as u64,
            actual: LIMIT as u64 + 1
        })
    );
    // Exactly at the limit it is read and judged like any other list: this one is signed and says nothing.
    let mut release = good();
    resign(&mut release, "a".repeat(LIMIT));
    assert_eq!(check(&release), Err(ImageError::SumsMalformed { line: 1 }));
    // The manifest has the same limit.
    let mut release = good();
    release.files.insert(MANIFEST_NAME, vec![b' '; LIMIT + 1]);
    assert_eq!(
        check(&release),
        Err(ImageError::WrongSize {
            name: MANIFEST_NAME.into(),
            expected: LIMIT as u64,
            actual: LIMIT as u64 + 1
        })
    );
}

#[test]
fn a_bad_line_of_the_list_is_named_by_its_number() {
    let good_line = |name: &str| format!("{}  {name}\n", "a".repeat(64));
    for (sums, line) in [
        ("no hash here\n".to_string(), 1),
        (good_line("one") + "broken\n", 2),
        (good_line("one") + &good_line("two") + &good_line("one"), 3),
        (good_line("one") + "\n" + &good_line("two"), 2),
        ("\u{feff}".to_string() + &good_line("one"), 1),
    ] {
        let mut release = good();
        resign(&mut release, &sums);
        assert_eq!(
            check(&release),
            Err(ImageError::SumsMalformed { line }),
            "{sums:?}"
        );
    }
    // Bytes that are no text at all.
    let mut release = good();
    resign(&mut release, [0xff, 0xfe, b'\n']);
    assert_eq!(check(&release), Err(ImageError::SumsMalformed { line: 1 }));
}

#[test]
fn boundaries_of_the_part_size() {
    // Exactly two parts, no trailing empty part; an empty root filesystem is one empty part.
    for (rootfs, parts) in [
        (vec![9u8; 2000], 2),
        (vec![9u8; 2001], 3),
        (vec![9u8; 1], 1),
        (vec![], 1),
        (vec![9u8; 1000], 1),
    ] {
        let release = release_with(b"k", b"i", Some(&rootfs), 1000);
        let verified = verify(&release.files, &release.key.verifying_key(), VERSION, 0).unwrap();
        assert_eq!(
            verified.manifest.rootfs.unwrap().parts.len(),
            parts,
            "{} bytes",
            rootfs.len()
        );
    }
}

#[test]
fn a_folder_on_disk_verifies_like_memory_and_a_missing_folder_is_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("Image"), b"kernel").unwrap();
    fs::write(src.path().join("initramfs"), b"initramfs").unwrap();
    let signing = key(1);
    write_release(
        dir.path(),
        &ReleaseInputs {
            version: VERSION,
            built: BUILT,
            kernel: &src.path().join("Image"),
            initramfs: &src.path().join("initramfs"),
            rootfs: None,
            max_part: 10,
        },
        &signing,
    )
    .unwrap();
    assert!(verify(&DirFiles::new(dir.path()), &signing.verifying_key(), VERSION, 0).is_ok());
    assert_eq!(
        verify(
            &DirFiles::new(dir.path().join("nope")),
            &signing.verifying_key(),
            VERSION,
            0
        )
        .map(|_| ()),
        Err(ImageError::Missing(SUMS_NAME.into()))
    );
}

fn inputs<'a>(kernel: &'a std::path::Path, initramfs: &'a std::path::Path) -> ReleaseInputs<'a> {
    ReleaseInputs {
        version: VERSION,
        built: BUILT,
        kernel,
        initramfs,
        rootfs: None,
        max_part: 1000,
    }
}

#[test]
fn a_release_is_not_written_over_its_own_sources() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("Image"), b"kernel").unwrap();
    fs::write(dir.path().join("initramfs"), b"initramfs").unwrap();
    let error = write_release(
        dir.path(),
        &inputs(&dir.path().join("Image"), &dir.path().join("initramfs")),
        &key(7),
    )
    .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{error}");
    assert_eq!(fs::read(dir.path().join("Image")).unwrap(), b"kernel");
}

#[test]
fn files_that_cannot_be_told_apart_or_collide_with_the_release_are_refused() {
    let out = tempfile::tempdir().unwrap();
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    for (dir, name) in [(&a, "same"), (&b, "same"), (&a, "image.json"), (&a, "other")] {
        fs::write(dir.path().join(name), b"x").unwrap();
    }
    for (kernel, initramfs) in [
        (a.path().join("same"), b.path().join("same")),
        (a.path().join("image.json"), a.path().join("other")),
    ] {
        let error = write_release(out.path(), &inputs(&kernel, &initramfs), &key(7)).unwrap_err();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "{kernel:?}: {error}"
        );
    }
}

#[test]
fn a_shorter_rootfs_leaves_no_part_of_the_earlier_release_behind() {
    let out = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    for (name, bytes) in [("Image", 4), ("initramfs", 4), ("rootfs.erofs", 2500)] {
        fs::write(src.path().join(name), vec![5u8; bytes]).unwrap();
    }
    let (kernel, initramfs, rootfs) = (
        src.path().join("Image"),
        src.path().join("initramfs"),
        src.path().join("rootfs.erofs"),
    );
    let mut release = inputs(&kernel, &initramfs);
    release.rootfs = Some(&rootfs);
    write_release(out.path(), &release, &key(7)).unwrap();
    assert!(out.path().join("rootfs.erofs.002").exists());
    fs::write(&rootfs, vec![6u8; 1500]).unwrap();
    write_release(out.path(), &release, &key(7)).unwrap();
    assert!(!out.path().join("rootfs.erofs.002").exists());
    verify(&DirFiles::new(out.path()), &key(7).verifying_key(), VERSION, 0).unwrap();
}

#[test]
fn keys_round_trip_and_bad_keys_are_refused() {
    let signing = key(3);
    let text = URL_SAFE_NO_PAD.encode([3u8; 32]);
    assert_eq!(parse_signing_key(&text).unwrap().to_bytes(), signing.to_bytes());
    assert_eq!(
        cf_image::parse_public_key(&public_key_text(&signing)).unwrap(),
        signing.verifying_key()
    );
    for bad in [
        "",
        "AAAA",
        "!!!",
        &URL_SAFE_NO_PAD.encode([1u8; 31]),
        &URL_SAFE_NO_PAD.encode([1u8; 33]),
    ] {
        assert!(
            matches!(parse_signing_key(bad), Err(ImageError::KeyMalformed(_))),
            "{bad:?}"
        );
        assert!(
            matches!(cf_image::parse_public_key(bad), Err(ImageError::KeyMalformed(_))),
            "{bad:?}"
        );
    }
}

/// Changes `image.json` and signs the result, as someone holding the key would (to reach the checks on a manifest
/// that is genuine but wrong).
fn rewrite_manifest(release: &mut Release, change: impl FnOnce(&mut serde_json::Value)) {
    let old = release.files.get(MANIFEST_NAME).unwrap().to_vec();
    let mut value: serde_json::Value = serde_json::from_slice(&old).unwrap();
    change(&mut value);
    let new = serde_json::to_vec_pretty(&value).unwrap();
    assert_ne!(old, new, "the change changed nothing");
    let sums = text_of(release, SUMS_NAME).replace(&hex(&Sha256::digest(&old)), &hex(&Sha256::digest(&new)));
    release.files.insert(MANIFEST_NAME, new);
    resign(release, &sums);
}

#[test]
fn an_older_release_is_refused_when_a_newer_one_was_accepted() {
    let release = good();
    let with_floor = |floor| {
        verify(&release.files, &release.key.verifying_key(), VERSION, floor).map(|v| v.manifest.built)
    };
    assert_eq!(with_floor(0), Ok(BUILT));
    assert_eq!(with_floor(BUILT - 1), Ok(BUILT));
    // The same release again is not a step back.
    assert_eq!(with_floor(BUILT), Ok(BUILT));
    for floor in [BUILT + 1, u64::MAX] {
        assert_eq!(
            with_floor(floor),
            Err(ImageError::Older {
                built: BUILT,
                newest_accepted: floor
            })
        );
    }
}

#[test]
fn a_release_that_names_no_build_time_is_older_than_any_accepted_one() {
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| {
        manifest.as_object_mut().unwrap().remove("built");
    });
    let with_floor = |floor| verify(&release.files, &release.key.verifying_key(), VERSION, floor).map(|_| ());
    assert_eq!(with_floor(0), Ok(()));
    assert_eq!(
        with_floor(1),
        Err(ImageError::Older {
            built: 0,
            newest_accepted: 1
        })
    );
}

#[test]
fn the_build_time_cannot_be_moved_without_the_key() {
    let mut release = good();
    let text = text_of(&release, MANIFEST_NAME);
    let younger = text.replace(&BUILT.to_string(), &(BUILT + 1_000_000).to_string());
    assert_ne!(text, younger);
    release.files.insert(MANIFEST_NAME, younger);
    assert_eq!(
        check(&release),
        Err(ImageError::HashMismatch(MANIFEST_NAME.into()))
    );
}

#[test]
fn a_list_signed_for_something_else_is_no_image() {
    // The key is asked to sign a list of other files (a program's update, say): it must not pass as an image,
    // whatever else is in the folder.
    let mut release = good();
    let sums = format!("{}  codefusion-runner-aarch64-apple-darwin\n", "a".repeat(64));
    resign(&mut release, &sums);
    assert_eq!(
        check(&release),
        Err(ImageError::HashMismatch(MANIFEST_NAME.into()))
    );
    // An empty list, signed.
    let mut release = good();
    resign(&mut release, "");
    assert_eq!(
        check(&release),
        Err(ImageError::HashMismatch(MANIFEST_NAME.into()))
    );
}

#[test]
fn a_signed_manifest_that_names_a_file_twice_or_outside_the_folder_is_refused() {
    // The same part twice: each line of the list still holds, but the whole would be read from one file twice.
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| {
        let parts = manifest["rootfs"]["parts"].as_array_mut().unwrap();
        parts[1] = parts[0].clone();
    });
    assert_eq!(
        check(&release),
        Err(ImageError::ManifestDisagrees("rootfs.erofs.000".into()))
    );

    // The kernel's file given as the initramfs too.
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| {
        manifest["initramfs"] = manifest["kernel"].clone();
    });
    assert_eq!(
        check(&release),
        Err(ImageError::ManifestDisagrees("Image".into()))
    );

    // A part that is the manifest itself.
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| {
        manifest["rootfs"]["parts"][0]["name"] = MANIFEST_NAME.into();
    });
    assert_eq!(
        check(&release),
        Err(ImageError::ManifestDisagrees(MANIFEST_NAME.into()))
    );

    for name in ["../Image", "/etc/hosts", "a/Image", "..", "", "Image\n"] {
        let mut release = good();
        rewrite_manifest(&mut release, |manifest| manifest["kernel"]["name"] = name.into());
        assert_eq!(
            check(&release),
            Err(ImageError::ManifestDisagrees(name.into())),
            "{name:?}"
        );
    }
}

#[test]
fn part_sizes_no_number_can_hold_are_refused_without_a_crash() {
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| {
        manifest["rootfs"]["parts"][0]["size"] = u64::MAX.into();
        manifest["rootfs"]["parts"][1]["size"] = u64::MAX.into();
    });
    assert_eq!(check(&release), Err(ImageError::RootfsMismatch));

    // Sizes that add up to the whole but are not the files' sizes.
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| {
        manifest["rootfs"]["parts"][0]["size"] = 999.into();
        manifest["rootfs"]["parts"][1]["size"] = 1001.into();
    });
    assert_eq!(
        check(&release),
        Err(ImageError::WrongSize {
            name: "rootfs.erofs.000".into(),
            expected: 999,
            actual: 1000
        })
    );

    // A whole of another size than its parts.
    let mut release = good();
    rewrite_manifest(&mut release, |manifest| manifest["rootfs"]["size"] = 2501.into());
    assert_eq!(check(&release), Err(ImageError::RootfsMismatch));
}

#[test]
fn a_signature_only_a_lenient_check_accepts_is_refused() {
    // The point of order 1 as the key and as R, with S = 0, satisfies the Ed25519 equation for every message. A
    // key like that signs nothing; only the strict check says so.
    let mut identity = [0u8; 32];
    identity[0] = 1;
    let weak = cf_image::parse_public_key(&URL_SAFE_NO_PAD.encode(identity)).unwrap();
    let mut signature = [0u8; 64];
    signature[0] = 1;
    let mut release = good();
    release
        .files
        .insert(SIGNATURE_NAME, URL_SAFE_NO_PAD.encode(signature));
    assert_eq!(
        verify(&release.files, &weak, VERSION, 0).map(|_| ()),
        Err(ImageError::BadSignature)
    );
}

#[test]
fn a_signature_file_may_end_with_a_newline_but_holds_nothing_else() {
    let mut release = good();
    let mut signature = release.files.get(SIGNATURE_NAME).unwrap().to_vec();
    signature.push(b'\n');
    release.files.insert(SIGNATURE_NAME, signature.clone());
    assert_eq!(check(&release), Ok(()));
    // Padded, or with something after it.
    for tail in ["==", "A", " x"] {
        let mut longer = signature.clone();
        longer.pop();
        longer.extend_from_slice(tail.as_bytes());
        release.files.insert(SIGNATURE_NAME, longer);
        assert_eq!(check(&release), Err(ImageError::SignatureMalformed), "{tail:?}");
    }
}

#[cfg(unix)]
#[test]
fn a_folder_gives_only_its_own_plain_files() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("Image"), b"kernel").unwrap();
    fs::write(src.path().join("initramfs"), b"initramfs").unwrap();
    let signing = key(1);
    write_release(
        dir.path(),
        &inputs(&src.path().join("Image"), &src.path().join("initramfs")),
        &signing,
    )
    .unwrap();
    let check = || verify(&DirFiles::new(dir.path()), &signing.verifying_key(), VERSION, 0).map(|_| ());
    assert_eq!(check(), Ok(()));

    // The kernel as a link to a file with the right bytes somewhere else: what is there may change later.
    fs::rename(dir.path().join("Image"), elsewhere.path().join("Image")).unwrap();
    symlink(elsewhere.path().join("Image"), dir.path().join("Image")).unwrap();
    assert!(
        matches!(check(), Err(ImageError::Io { ref name, .. }) if name == "Image"),
        "{:?}",
        check()
    );

    // A folder where the kernel should be.
    fs::remove_file(dir.path().join("Image")).unwrap();
    fs::create_dir(dir.path().join("Image")).unwrap();
    assert!(
        matches!(check(), Err(ImageError::Io { ref name, .. }) if name == "Image"),
        "{:?}",
        check()
    );

    // A pipe nobody writes to must not hold the check up.
    fs::remove_dir(dir.path().join("Image")).unwrap();
    let pipe = std::ffi::CString::new(dir.path().join("Image").to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL terminated path and a mode.
    assert_eq!(unsafe { libc::mkfifo(pipe.as_ptr(), 0o600) }, 0);
    assert!(
        matches!(check(), Err(ImageError::Io { ref name, .. }) if name == "Image"),
        "{:?}",
        check()
    );

    // A name that leaves the folder is refused before the file system is asked.
    fs::write(elsewhere.path().join("secret"), b"x").unwrap();
    let outside = format!(
        "../{}/secret",
        elsewhere.path().file_name().unwrap().to_str().unwrap()
    );
    for name in [outside.as_str(), "..", "", "a/b"] {
        let error = cf_image::Files::open(&DirFiles::new(dir.path()), name)
            .map(|_| ())
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput, "{name:?}");
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// Any content and part size round-trips: it verifies and the parts join to the original.
    #[test]
    fn any_release_verifies_and_the_parts_join_to_the_original(
        kernel in proptest::collection::vec(any::<u8>(), 0..300),
        rootfs in proptest::collection::vec(any::<u8>(), 0..600),
        max_part in 1u64..200,
    ) {
        let release = release_with(&kernel, b"i", Some(&rootfs), max_part);
        let verified = verify(&release.files, &release.key.verifying_key(), VERSION, 0).unwrap();
        let mut joined = Vec::new();
        for part in verified.manifest.rootfs.unwrap().parts {
            prop_assert!(part.size <= max_part);
            joined.extend_from_slice(release.files.get(&part.name).unwrap());
        }
        prop_assert_eq!(joined, rootfs);
    }

    /// Flipping any single bit of any file of a release makes it fail.
    #[test]
    fn no_single_bit_flip_goes_unnoticed(
        rootfs in proptest::collection::vec(any::<u8>(), 1..300),
        file in 0usize..8,
        position in any::<usize>(),
        bit in 0u8..8,
    ) {
        let mut release = release_with(&[5; 40], &[6; 40], Some(&rootfs), 100);
        let names = [MANIFEST_NAME, SUMS_NAME, SIGNATURE_NAME, "Image", "initramfs.cpio.gz", "rootfs.erofs.000"];
        let name = names[file % names.len()];
        let bytes = release.files.get_mut(name).unwrap();
        let at = position % bytes.len();
        bytes[at] ^= 1 << bit;
        prop_assert!(check(&release).is_err(), "{name} byte {at} bit {bit}");
    }

    /// Whatever bytes stand where the list of hashes should be, signed or not, the answer is a refusal and never
    /// a crash.
    #[test]
    fn any_bytes_as_the_list_are_refused(
        sums in proptest::collection::vec(any::<u8>(), 0..300),
        signed in any::<bool>(),
    ) {
        let mut release = good();
        if signed {
            resign(&mut release, &sums);
        } else {
            release.files.insert(SUMS_NAME, sums);
        }
        prop_assert!(check(&release).is_err());
    }

    /// The same for lines made of what a list is made of: hashes, names, spaces and line ends in any order.
    #[test]
    fn any_lines_as_the_list_are_refused(
        pieces in proptest::collection::vec(
            prop_oneof![
                Just("a".repeat(64)),
                Just("  ".to_string()),
                Just(" ".to_string()),
                Just("\n".to_string()),
                Just("\r\n".to_string()),
                Just("image.json".to_string()),
                Just("Image".to_string()),
                Just("../".to_string()),
                Just("\0".to_string()),
                "[ -~]{0,8}",
            ],
            0..12,
        ),
    ) {
        let mut release = good();
        let sums = pieces.concat();
        resign(&mut release, &sums);
        prop_assert!(check(&release).is_err(), "{sums:?}");
    }

    /// Any bytes as a signed manifest: refused, never a crash.
    #[test]
    fn any_bytes_as_the_manifest_are_refused(manifest in proptest::collection::vec(any::<u8>(), 0..300)) {
        let mut release = good();
        let sums = format!("{}  {MANIFEST_NAME}\n", hex(&Sha256::digest(&manifest)));
        release.files.insert(MANIFEST_NAME, manifest);
        resign(&mut release, &sums);
        prop_assert!(check(&release).is_err());
    }

    /// Any numbers and names in a signed manifest of the right shape: refused unless they are the true ones.
    #[test]
    fn a_manifest_with_any_sizes_and_names_is_refused(
        sizes in proptest::collection::vec(any::<u64>(), 3),
        name in "[ -~]{0,12}",
        which in 0usize..4,
    ) {
        let mut release = good();
        rewrite_manifest(&mut release, |manifest| {
            let parts = manifest["rootfs"]["parts"].as_array_mut().unwrap();
            for (part, size) in parts.iter_mut().zip(&sizes) {
                // Never the true size, so the manifest is always wrong.
                part["size"] = (if *size == part["size"].as_u64().unwrap() { size ^ 1 } else { *size }).into();
            }
            match which {
                0 => manifest["kernel"]["name"] = name.clone().into(),
                1 => manifest["initramfs"]["name"] = name.clone().into(),
                2 => manifest["rootfs"]["parts"][0]["name"] = name.clone().into(),
                _ => {}
            }
        });
        prop_assert!(check(&release).is_err());
    }
}

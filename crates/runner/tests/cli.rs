//! The command line as a person meets it.

use std::fs;
use std::process::Command;

use cf_image::{ReleaseInputs, write_release};
use ed25519_dalek::SigningKey;

fn runner(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_codefusion-runner"))
        .args(args)
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn a_release_signed_by_a_key_that_is_not_the_trusted_one_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    fs::write(src.path().join("Image"), b"kernel").unwrap();
    fs::write(src.path().join("initramfs"), b"initramfs").unwrap();
    write_release(
        dir.path(),
        &ReleaseInputs {
            version: "v1",
            built: 0,
            kernel: &src.path().join("Image"),
            initramfs: &src.path().join("initramfs"),
            rootfs: None,
            max_part: 100,
        },
        &SigningKey::from_bytes(&[9; 32]),
    )
    .unwrap();
    let (ok, text) = runner(&["image", "verify", dir.path().to_str().unwrap(), "--version", "v1"]);
    assert!(!ok);
    assert!(
        text.contains("refused: the signature of SHA256SUMS is not valid for the trusted key"),
        "{text}"
    );
}

#[test]
fn an_empty_folder_and_bad_usage_are_refused_with_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let (ok, text) = runner(&["image", "verify", dir.path().to_str().unwrap(), "--version", "v1"]);
    assert!(!ok);
    assert!(text.contains("SHA256SUMS is missing"), "{text}");
    let (ok, text) = runner(&["frobnicate"]);
    assert!(!ok);
    assert!(text.contains("usage:"), "{text}");
}

#[test]
fn the_version_command_names_the_package_version() {
    let (ok, text) = runner(&["version"]);
    assert!(ok);
    assert_eq!(
        text.trim(),
        format!("codefusion-runner {}", env!("CARGO_PKG_VERSION"))
    );
}

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
fn the_version_command_names_the_package_version_and_the_commit_it_was_built_at() {
    let (ok, text) = runner(&["version"]);
    assert!(ok);
    // The commit of this checkout, which the program under test was just built from, and whether what goes into
    // the program differs from it; the name the build was given instead; `unknown` where no git is.
    let git = |args: &[&str]| {
        Command::new("git")
            .args(["-C", concat!(env!("CARGO_MANIFEST_DIR"), "/../..")])
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    let commit = match std::env::var("CF_COMMIT") {
        Ok(named) if !named.trim().is_empty() => named.trim().to_string(),
        _ => git(&["rev-parse", "--short=12", "HEAD"]).map_or("unknown".to_string(), |commit| {
            assert_eq!(commit.len(), 12, "{commit}");
            let changed = git(&[
                "status",
                "--porcelain",
                "--untracked-files=no",
                "--",
                "crates",
                ":(glob,exclude)crates/*/tests/**",
                "Cargo.toml",
                "Cargo.lock",
            ]);
            if changed.is_some_and(|changed| !changed.is_empty()) {
                format!("{commit}-dirty")
            } else {
                commit
            }
        }),
    };
    assert_eq!(
        text.trim(),
        format!("codefusion-runner {}+{commit}", env!("CARGO_PKG_VERSION"))
    );
}

#[cfg(target_os = "macos")]
#[test]
fn a_boot_proof_takes_no_time_that_no_clock_can_add() {
    for timeout in ["18446744073709551615", "3601", "0"] {
        let (ok, text) = runner(&["boot-proof", "--timeout-s", timeout]);
        assert!(!ok);
        assert!(text.contains("usage:"), "{timeout}: {text}");
        assert!(!text.contains("panicked"), "{timeout}: {text}");
    }
}

//! The bundle a build embeds: present and intact when `cargo xtask guest` ran, absent otherwise.
//! CI sets `CF_REQUIRE_EMBEDDED=1` after building the guest, so a missing bundle there is a failure.

use cf_image::{ImageError, embedded_bundle, verify, verify_embedded};

#[test]
fn the_embedded_bundle_is_there_when_required_and_verifies() {
    let required = std::env::var_os("CF_REQUIRE_EMBEDDED").is_some();
    match embedded_bundle() {
        None => {
            assert!(
                !required,
                "CF_REQUIRE_EMBEDDED is set but this build embeds no guest image"
            );
            assert_eq!(verify_embedded().unwrap_err(), ImageError::NoEmbeddedImage);
        }
        Some(bundle) => {
            let verified = verify_embedded().unwrap();
            assert_eq!(verified.manifest.version, bundle.version);
            assert!(verified.manifest.rootfs.is_none());
            // The kernel is this host's, whatever the manifest says: a plain arm64 Image, or the x86_64 vmlinux ELF.
            assert_eq!(verified.manifest.arch, cf_image::HOST_ARCH);
            let mut files = bundle.mem_files();
            let kernel = bundle.file(&verified.manifest.kernel.name).unwrap();
            match cf_image::HOST_ARCH {
                "aarch64" => assert_eq!(&kernel[56..60], b"ARM\x64"),
                "x86_64" => {
                    assert_eq!(&kernel[..6], b"\x7fELF\x02\x01");
                    assert_eq!(&kernel[18..20], 62u16.to_le_bytes());
                }
                other => panic!("no guest is built for {other}"),
            }

            // One changed byte in the embedded kernel is refused.
            files.get_mut(&verified.manifest.kernel.name).unwrap()[100] ^= 1;
            assert_eq!(
                verify(&files, &bundle.verifying_key().unwrap(), bundle.version, 0).unwrap_err(),
                ImageError::HashMismatch(verified.manifest.kernel.name.clone())
            );
        }
    }
}

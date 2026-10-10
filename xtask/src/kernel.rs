//! The guest kernel: Alpine's `linux-virt` for aarch64, pinned by the SHA-256 of the package.
//!
//! Alpine ships the kernel as an EFI "zboot" image holding a gzip-compressed arm64 `Image`.
//! Virtualization.framework boots only the plain `Image`, so it is unpacked here.
//!
//! The pin is the check a build makes; HTTPS only carries the bytes. The pinned package was checked once against
//! Alpine's own signature (2026-10-10, key `alpine-devel@lists.alpinelinux.org-616ae350.rsa.pub`, the same file
//! from alpinelinux.org/keys and from the aports repository). To move the pin, check the new package the same way
//! before its hash goes in here. An apk is three gzip streams: a tar with the signature, a tar with `.PKGINFO`,
//! and the files.
//!
//! 1. `openssl dgst -sha1 -verify <key> -signature <the .SIGN.RSA file of stream 1> <stream 2 as it is>` says
//!    `Verified OK`.
//! 2. `datahash` in `.PKGINFO` is the SHA-256 of stream 3 as it is.
//! 3. `pkgname`, `pkgver` and `arch` in `.PKGINFO` are the ones wanted.

use std::io::Read;

use flate2::read::GzDecoder;

use crate::Result;

/// Alpine's package of the kernel and its modules.
pub const APK_URL: &str =
    "https://dl-cdn.alpinelinux.org/alpine/v3.24/main/aarch64/linux-virt-6.18.55-r0.apk";
/// SHA-256 of that package, which pins the kernel and every module.
pub const APK_SHA256: &str = "c7fb892408d7fe163a18671e5c7816752976d1c67fd17794dfba794aa0d6c1ac";
/// The package's version, part of the bundle's version.
pub const APK_VERSION: &str = "6.18.55-r0";

/// The arm64 `Image` header carries "ARM\x64" at offset 56.
const ARM64_MAGIC: &[u8] = b"ARM\x64";

/// Unpacks an EFI zboot image (`zimg` header, a payload offset and size, a compression name) to the plain kernel.
pub fn unpack_zboot(image: &[u8]) -> Result<Vec<u8>> {
    if image.len() < 32 || &image[0..2] != b"MZ" || &image[4..8] != b"zimg" {
        return Err("the kernel is not an EFI zboot image".into());
    }
    let offset = u32::from_le_bytes(image[8..12].try_into()?) as usize;
    let size = u32::from_le_bytes(image[12..16].try_into()?) as usize;
    let compression = &image[24..32];
    if !compression.starts_with(b"gzip") {
        return Err(format!(
            "the kernel payload is compressed with {:?}, not gzip",
            String::from_utf8_lossy(compression)
        )
        .into());
    }
    let payload = offset
        .checked_add(size)
        .and_then(|end| image.get(offset..end))
        .ok_or("the zboot payload is cut off")?;
    let mut kernel = Vec::new();
    GzDecoder::new(payload).read_to_end(&mut kernel)?;
    if kernel.get(56..60) != Some(ARM64_MAGIC) {
        return Err("the unpacked kernel is not an arm64 Image".into());
    }
    Ok(kernel)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::{Compression, write::GzEncoder};

    use super::*;

    fn zboot(payload_kernel: &[u8], compression: &[u8; 8]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        gz.write_all(payload_kernel).unwrap();
        let payload = gz.finish().unwrap();
        let mut image = vec![0u8; 64];
        image[0..2].copy_from_slice(b"MZ");
        image[4..8].copy_from_slice(b"zimg");
        image[8..12].copy_from_slice(&64u32.to_le_bytes());
        image[12..16].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        image[24..32].copy_from_slice(compression);
        image.extend(payload);
        image
    }

    fn kernel() -> Vec<u8> {
        let mut kernel = vec![0u8; 128];
        kernel[56..60].copy_from_slice(ARM64_MAGIC);
        kernel
    }

    #[test]
    fn a_zboot_image_unpacks_to_the_kernel() {
        assert_eq!(
            unpack_zboot(&zboot(&kernel(), b"gzip\0\0\0\0")).unwrap(),
            kernel()
        );
    }

    #[test]
    fn what_is_not_a_gzip_zboot_arm64_image_is_refused() {
        assert!(unpack_zboot(b"short").is_err());
        assert!(unpack_zboot(&zboot(&kernel(), b"zstd22\0\0")).is_err());
        assert!(unpack_zboot(&zboot(&[0u8; 128], b"gzip\0\0\0\0")).is_err());
        let mut cut = zboot(&kernel(), b"gzip\0\0\0\0");
        cut.truncate(70);
        assert!(unpack_zboot(&cut).is_err());
        let mut lying = zboot(&kernel(), b"gzip\0\0\0\0");
        lying[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(unpack_zboot(&lying).is_err());
    }
}

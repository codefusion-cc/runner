use ed25519_dalek::VerifyingKey;

use crate::error::ImageError;
use crate::files::MemFiles;
use crate::verify::{Verified, parse_public_key, verify};

include!(concat!(env!("OUT_DIR"), "/embedded.rs"));

/// The kernel, the initramfs and their signed manifest that this build carries.
pub struct Bundle {
    /// The release version the bundle was built as.
    pub version: &'static str,
    /// The files by name: `image.json`, `SHA256SUMS`, `SHA256SUMS.sig`, the kernel and the initramfs.
    pub files: &'static [(&'static str, &'static [u8])],
    /// The key the bundle was signed with, made for that build. It is not the key that signs image releases.
    pub key: &'static str,
}

/// The bundle this build embeds, or `None` when it was built without `cargo xtask guest`.
pub fn embedded_bundle() -> Option<Bundle> {
    EMBEDDED
}

impl Bundle {
    /// One of the bundle's files.
    pub fn file(&self, name: &str) -> Option<&'static [u8]> {
        self.files
            .iter()
            .find(|(file, _)| *file == name)
            .map(|(_, bytes)| *bytes)
    }

    /// The bundle's files as a set that [`verify`] reads.
    pub fn mem_files(&self) -> MemFiles {
        let mut all = MemFiles::new();
        for (name, bytes) in self.files {
            all.insert(name, *bytes);
        }
        all
    }

    /// The key as a verifying key.
    pub fn verifying_key(&self) -> Result<VerifyingKey, ImageError> {
        parse_public_key(self.key)
    }
}

/// Verifies the embedded kernel and initramfs against their embedded manifest. Catches a damaged or truncated
/// binary; it is no defence against someone who can rewrite the binary, who can rewrite the key as well.
pub fn verify_embedded() -> Result<Verified, ImageError> {
    let bundle = embedded_bundle().ok_or(ImageError::NoEmbeddedImage)?;
    verify(&bundle.mem_files(), &bundle.verifying_key()?, bundle.version, 0)
}

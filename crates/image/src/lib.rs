//! The guest image: a kernel, an initramfs and a root filesystem, published as files next to a signed list of their
//! hashes.
//!
//! A release is a folder of files:
//!
//! - `image.json`, the [`Manifest`]: the version and each file's name, size and SHA-256;
//! - the kernel, the initramfs and the root filesystem (in parts when it is larger than a release asset may be);
//! - `SHA256SUMS`, one `<hex>  <name>` line per file above;
//! - `SHA256SUMS.sig`, an Ed25519 signature of `SHA256SUMS` as unpadded base64url (Magnetar's scheme).
//!
//! [`verify`] checks it in that order, so nothing is read or parsed before the signature holds. Everything here
//! returns an [`ImageError`] for a bad image: a hostile or damaged download is a result, not a panic.

mod embedded;
mod error;
mod files;
mod manifest;
mod release;
#[cfg(unix)]
pub mod store;
mod verify;

pub use embedded::{Bundle, embedded_bundle, verify_embedded};
pub use error::ImageError;
pub use files::{DirFiles, Files, MemFiles};
pub use manifest::{Asset, FORMAT, HOST_ARCH, MANIFEST_NAME, Manifest, Rootfs, SIGNATURE_NAME, SUMS_NAME};
pub use release::{ReleaseInputs, parse_signing_key, public_key_text, write_release};
pub use verify::{Verified, parse_public_key, verify};

/// The public key every downloaded image must be signed with (unpadded base64url), committed in `trusted-key.txt`.
pub const TRUSTED_PUBLIC_KEY: &str = include_str!("../trusted-key.txt");

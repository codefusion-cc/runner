use serde::{Deserialize, Serialize};

/// The manifest format this build reads and writes.
pub const FORMAT: u32 = 1;
/// The manifest's file name in a release.
pub const MANIFEST_NAME: &str = "image.json";
/// The list of hashes the signature covers.
pub const SUMS_NAME: &str = "SHA256SUMS";
/// The signature of [`SUMS_NAME`].
pub const SIGNATURE_NAME: &str = "SHA256SUMS.sig";

/// A release file is a plain name: no folder, no `.` or `..`, no control characters.
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// A file a manifest may name: a plain name that is none of the release's own three files. The writer and the
/// verification both ask here, so they cannot come to disagree.
pub(crate) fn valid_asset_name(name: &str) -> bool {
    valid_name(name) && ![MANIFEST_NAME, SUMS_NAME, SIGNATURE_NAME].contains(&name)
}

/// One file of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    /// File name, a plain name with no folder.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Lowercase hex SHA-256.
    pub sha256: String,
}

/// The root filesystem, whole and in the parts it was published in (the order is the order to join them).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rootfs {
    /// Size of the whole in bytes.
    pub size: u64,
    /// SHA-256 of the whole.
    pub sha256: String,
    /// The parts, each at most as large as a release asset may be.
    pub parts: Vec<Asset>,
}

/// `image.json`: what a release is made of.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`FORMAT`].
    pub format: u32,
    /// The image's version, the release tag it is published under.
    pub version: String,
    /// When the release was built, in seconds since 1970 (UTC). Signed with the rest, so a machine that remembers
    /// the newest value it accepted can refuse an older release. 0 in the bundle a binary embeds, which is never
    /// downloaded.
    #[serde(default)]
    pub built: u64,
    /// The guest kernel, an uncompressed arm64 `Image`.
    pub kernel: Asset,
    /// The initramfs with the supervisor and its modules.
    pub initramfs: Asset,
    /// The root filesystem a job runs in (GitHub's Actions runner and its tools); not in the bundle a binary embeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rootfs: Option<Rootfs>,
}

impl Manifest {
    /// Every file this manifest names, in the order `SHA256SUMS` lists them after `image.json`.
    pub fn assets(&self) -> Vec<&Asset> {
        let mut all = vec![&self.kernel, &self.initramfs];
        if let Some(rootfs) = &self.rootfs {
            all.extend(rootfs.parts.iter());
        }
        all
    }
}

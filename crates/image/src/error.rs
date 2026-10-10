use thiserror::Error;

/// Why an image was refused. Every variant names what is wrong, so a person reading a log knows what to do.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ImageError {
    /// The signing or verifying key is not 32 bytes of unpadded base64url.
    #[error("the key is malformed: {0}")]
    KeyMalformed(String),
    /// `SHA256SUMS.sig` is not a base64url Ed25519 signature.
    #[error("the signature file is malformed")]
    SignatureMalformed,
    /// The signature does not match `SHA256SUMS` under the trusted key.
    #[error("the signature of SHA256SUMS is not valid for the trusted key")]
    BadSignature,
    /// A line of `SHA256SUMS` is not `<64 hex>  <name>`, names a path, or repeats a name.
    #[error("SHA256SUMS line {line} is malformed")]
    SumsMalformed { line: usize },
    /// `image.json` is not valid JSON of the expected shape.
    #[error("image.json is malformed: {0}")]
    ManifestMalformed(String),
    /// `image.json` is of a format this build does not know.
    #[error("image.json has format {found}, this build reads format {supported}")]
    UnknownFormat { found: u32, supported: u32 },
    /// The release is for another version than the one asked for (for example an older release under a newer tag).
    #[error("the release is version {found:?}, expected {expected:?}")]
    WrongVersion { expected: String, found: String },
    /// The release is genuine but was built before the one already accepted.
    #[error("the release was built at {built}, before the one in use ({newest_accepted})")]
    Older { built: u64, newest_accepted: u64 },
    /// A file the release needs is not there.
    #[error("{0} is missing")]
    Missing(String),
    /// A file the manifest names is not listed in `SHA256SUMS`, or lists another hash or size than the manifest.
    #[error("{0} does not match between image.json and SHA256SUMS")]
    ManifestDisagrees(String),
    /// A file is shorter or longer than the manifest says.
    #[error("{name} is {actual} bytes, expected {expected}")]
    WrongSize {
        name: String,
        expected: u64,
        actual: u64,
    },
    /// A file's SHA-256 is not the listed one.
    #[error("{0} does not match its SHA-256")]
    HashMismatch(String),
    /// The root filesystem's parts do not add up to the whole the manifest names.
    #[error("the root filesystem parts do not match the whole")]
    RootfsMismatch,
    /// This build embeds no guest image (it was built without `cargo xtask guest`).
    #[error("this build carries no guest image")]
    NoEmbeddedImage,
    /// Reading a file failed for a reason other than a missing file.
    #[error("reading {name}: {message}")]
    Io { name: String, message: String },
}

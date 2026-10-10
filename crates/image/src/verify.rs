use std::collections::BTreeMap;
use std::io::{ErrorKind, Read};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::error::ImageError;
use crate::files::Files;
use crate::manifest::{
    Asset, FORMAT, MANIFEST_NAME, Manifest, SIGNATURE_NAME, SUMS_NAME, valid_asset_name, valid_name,
};

/// A small file (`SHA256SUMS`, its signature, `image.json`) is never larger than this; a larger one is refused
/// before it is parsed.
const SMALL_FILE_LIMIT: u64 = 1 << 20;

/// An image that passed [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The manifest whose signature, hashes and sizes all held.
    pub manifest: Manifest,
}

/// Reads a public key written as unpadded base64url (the form `xtask keygen` prints).
pub fn parse_public_key(text: &str) -> Result<VerifyingKey, ImageError> {
    VerifyingKey::from_bytes(&decode_key(text)?).map_err(|error| ImageError::KeyMalformed(error.to_string()))
}

/// The 32 bytes of a key written as unpadded base64url, either half.
pub(crate) fn decode_key(text: &str) -> Result<[u8; 32], ImageError> {
    URL_SAFE_NO_PAD
        .decode(text.trim())
        .map_err(|error| ImageError::KeyMalformed(error.to_string()))?
        .try_into()
        .map_err(|_| ImageError::KeyMalformed("not 32 bytes".into()))
}

/// Verifies a release: the signature of `SHA256SUMS` first, then `image.json` against it, the version, and every
/// file's size and SHA-256 (the root filesystem's parts also as a whole). Nothing but the signature is looked at
/// before it holds. Large files are streamed, never held in memory.
///
/// Returns an error for anything that is not exactly the release `expected_version` signed by `key`, and for a
/// release built before `newest_accepted`: the [`Manifest::built`] of the newest image this machine has accepted,
/// 0 when it has accepted none. Without that an old release, genuine and signed, could be offered again under its
/// old name and bring back whatever was fixed since. The same release again passes.
///
/// What it proves is what the files held while it read them. Whoever uses them afterwards must own the folder
/// (nobody else may write there), or copy the bytes while hashing them and use the copy.
pub fn verify(
    files: &dyn Files,
    key: &VerifyingKey,
    expected_version: &str,
    newest_accepted: u64,
) -> Result<Verified, ImageError> {
    let sums = read_small(files, SUMS_NAME)?;
    let signature = read_small(files, SIGNATURE_NAME)?;
    let signature = URL_SAFE_NO_PAD
        .decode(String::from_utf8_lossy(&signature).trim())
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(ImageError::SignatureMalformed)?;
    key.verify_strict(&sums, &signature)
        .map_err(|_| ImageError::BadSignature)?;

    let listed = parse_sums(&sums)?;

    let manifest_bytes = read_small(files, MANIFEST_NAME)?;
    if listed.get(MANIFEST_NAME).map(String::as_str)
        != Some(format!("{:x}", Sha256::digest(&manifest_bytes)).as_str())
    {
        return Err(ImageError::HashMismatch(MANIFEST_NAME.into()));
    }
    let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| ImageError::ManifestMalformed(error.to_string()))?;
    if manifest.format != FORMAT {
        return Err(ImageError::UnknownFormat {
            found: manifest.format,
            supported: FORMAT,
        });
    }
    if manifest.version != expected_version {
        return Err(ImageError::WrongVersion {
            expected: expected_version.into(),
            found: manifest.version,
        });
    }
    if manifest.built < newest_accepted {
        return Err(ImageError::Older {
            built: manifest.built,
            newest_accepted,
        });
    }

    check_agreement(&manifest, &listed)?;

    for asset in [&manifest.kernel, &manifest.initramfs] {
        hash_asset(files, asset, &mut [])?;
    }
    if let Some(rootfs) = &manifest.rootfs {
        let total = rootfs
            .parts
            .iter()
            .try_fold(0u64, |total, part| total.checked_add(part.size));
        if total != Some(rootfs.size) {
            return Err(ImageError::RootfsMismatch);
        }
        let mut whole = Sha256::new();
        for part in &rootfs.parts {
            hash_asset(files, part, &mut [&mut whole])?;
        }
        if format!("{:x}", whole.finalize()) != rootfs.sha256 {
            return Err(ImageError::RootfsMismatch);
        }
    }
    Ok(Verified { manifest })
}

/// The manifest and the signed list must name the same files with the same hashes and sizes, and the list must
/// name nothing else.
fn check_agreement(manifest: &Manifest, listed: &BTreeMap<String, String>) -> Result<(), ImageError> {
    let mut seen = vec![MANIFEST_NAME];
    for asset in manifest.assets() {
        if !valid_asset_name(&asset.name)
            || seen.contains(&asset.name.as_str())
            || listed.get(&asset.name) != Some(&asset.sha256)
        {
            return Err(ImageError::ManifestDisagrees(asset.name.clone()));
        }
        seen.push(&asset.name);
    }
    if let Some(extra) = listed.keys().find(|name| !seen.contains(&name.as_str())) {
        return Err(ImageError::ManifestDisagrees(extra.clone()));
    }
    Ok(())
}

/// Streams one asset, checking its size and hash; `also` receives the same bytes (for the whole root filesystem).
fn hash_asset(files: &dyn Files, asset: &Asset, also: &mut [&mut Sha256]) -> Result<(), ImageError> {
    let mut reader = open(files, &asset.name)?.take(asset.size.saturating_add(1));
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| io_error(&asset.name, &error))?;
        if read == 0 {
            break;
        }
        size += read as u64;
        hasher.update(&buffer[..read]);
        for other in also.iter_mut() {
            other.update(&buffer[..read]);
        }
    }
    if size != asset.size {
        return Err(ImageError::WrongSize {
            name: asset.name.clone(),
            expected: asset.size,
            actual: size,
        });
    }
    if format!("{:x}", hasher.finalize()) != asset.sha256 {
        return Err(ImageError::HashMismatch(asset.name.clone()));
    }
    Ok(())
}

fn parse_sums(sums: &[u8]) -> Result<BTreeMap<String, String>, ImageError> {
    let text = std::str::from_utf8(sums).map_err(|_| ImageError::SumsMalformed { line: 1 })?;
    let mut listed = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        let (hash, name) = line
            .split_once("  ")
            .ok_or(ImageError::SumsMalformed { line: line_number })?;
        let hash_ok = hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !hash_ok || !valid_name(name) || listed.insert(name.to_string(), hash.to_string()).is_some() {
            return Err(ImageError::SumsMalformed { line: line_number });
        }
    }
    Ok(listed)
}

fn read_small(files: &dyn Files, name: &str) -> Result<Vec<u8>, ImageError> {
    let mut bytes = Vec::new();
    open(files, name)?
        .take(SMALL_FILE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error(name, &error))?;
    if bytes.len() as u64 > SMALL_FILE_LIMIT {
        return Err(ImageError::WrongSize {
            name: name.into(),
            expected: SMALL_FILE_LIMIT,
            actual: bytes.len() as u64,
        });
    }
    Ok(bytes)
}

fn open<'a>(files: &'a dyn Files, name: &str) -> Result<Box<dyn Read + 'a>, ImageError> {
    files.open(name).map_err(|error| match error.kind() {
        ErrorKind::NotFound => ImageError::Missing(name.into()),
        _ => io_error(name, &error),
    })
}

fn io_error(name: &str, error: &std::io::Error) -> ImageError {
    ImageError::Io {
        name: name.into(),
        message: error.to_string(),
    }
}

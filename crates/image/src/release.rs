use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};

use crate::error::ImageError;
use crate::manifest::{
    Asset, FORMAT, MANIFEST_NAME, Manifest, Rootfs, SIGNATURE_NAME, SUMS_NAME, valid_asset_name,
};
use crate::verify::decode_key;

/// What goes into a release.
pub struct ReleaseInputs<'a> {
    /// The image's version, also the release tag.
    pub version: &'a str,
    /// When the release is built, in seconds since 1970 (UTC); 0 for a bundle that is embedded, not downloaded.
    pub built: u64,
    /// The uncompressed kernel `Image`.
    pub kernel: &'a Path,
    /// The initramfs.
    pub initramfs: &'a Path,
    /// The root filesystem, when the release has one.
    pub rootfs: Option<&'a Path>,
    /// The largest size of one published part of the root filesystem (a release asset is at most 2 GiB).
    pub max_part: u64,
}

/// Reads a signing key written as unpadded base64url (a 32 byte seed).
pub fn parse_signing_key(text: &str) -> Result<SigningKey, ImageError> {
    Ok(SigningKey::from_bytes(&decode_key(text)?))
}

/// The public key of a signing key as unpadded base64url, the form `trusted-key.txt` holds.
pub fn public_key_text(key: &SigningKey) -> String {
    URL_SAFE_NO_PAD.encode(key.verifying_key().as_bytes())
}

/// Writes a signed release into `out`: the files (the root filesystem cut into parts), `image.json`, `SHA256SUMS`
/// and `SHA256SUMS.sig`. The folder is created if needed; files already there are replaced.
pub fn write_release(out: &Path, inputs: &ReleaseInputs<'_>, key: &SigningKey) -> io::Result<Manifest> {
    fs::create_dir_all(out)?;
    let kernel_name = plain_name(inputs.kernel)?;
    let initramfs_name = plain_name(inputs.initramfs)?;
    if kernel_name == initramfs_name {
        return Err(invalid(format!(
            "the kernel and the initramfs are both called {kernel_name}"
        )));
    }
    let sources = [Some(inputs.kernel), Some(inputs.initramfs), inputs.rootfs];
    for source in sources.into_iter().flatten() {
        refuse_inside(source, out)?;
    }
    let kernel = copy_asset(inputs.kernel, out, &kernel_name)?;
    let initramfs = copy_asset(inputs.initramfs, out, &initramfs_name)?;
    let rootfs = inputs
        .rootfs
        .map(|path| split_rootfs(path, out, inputs.max_part))
        .transpose()?;
    let manifest = Manifest {
        format: FORMAT,
        version: inputs.version.to_string(),
        built: inputs.built,
        kernel,
        initramfs,
        rootfs,
    };

    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(io::Error::other)?;
    fs::write(out.join(MANIFEST_NAME), &manifest_bytes)?;
    let mut sums = format!(
        "{}  {MANIFEST_NAME}\n",
        format_args!("{:x}", Sha256::digest(&manifest_bytes))
    );
    for asset in manifest.assets() {
        sums.push_str(&format!("{}  {}\n", asset.sha256, asset.name));
    }
    fs::write(out.join(SUMS_NAME), &sums)?;
    let signature = key.sign(sums.as_bytes());
    fs::write(
        out.join(SIGNATURE_NAME),
        URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    )?;
    Ok(manifest)
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// The file's name, which must be one `verify` accepts and none of the release's own files.
fn plain_name(path: &Path) -> io::Result<String> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| invalid("a file with no name".into()))?;
    if !valid_asset_name(name) {
        return Err(invalid(format!("{name:?} cannot be the name of a release file")));
    }
    Ok(name.to_string())
}

/// A source in the release folder would be truncated by its own copy, and the release would sign an empty file.
fn refuse_inside(source: &Path, out: &Path) -> io::Result<()> {
    let folder = fs::canonicalize(out)?;
    let parent = fs::canonicalize(source)?.parent().map(Path::to_path_buf);
    if parent.as_deref() == Some(folder.as_path()) {
        return Err(invalid(format!(
            "{} is inside the release folder; give a folder of its own",
            source.display()
        )));
    }
    Ok(())
}

fn copy_asset(from: &Path, out: &Path, name: &str) -> io::Result<Asset> {
    let mut reader = File::open(from)?;
    let mut writer = File::create(out.join(name))?;
    let (size, sha256) = copy_hashing(&mut reader, &mut writer, u64::MAX)?;
    Ok(Asset {
        name: name.to_string(),
        size,
        sha256,
    })
}

/// Cuts a file into `<name>.000`, `<name>.001`, ... of at most `max_part` bytes and hashes the whole and each part.
fn split_rootfs(from: &Path, out: &Path, max_part: u64) -> io::Result<Rootfs> {
    assert!(max_part > 0, "a part must hold at least one byte");
    let name = plain_name(from)?;
    remove_old_parts(out, &name)?;
    let mut reader = File::open(from)?;
    let mut whole = Sha256::new();
    let mut parts = Vec::new();
    loop {
        let part_name = format!("{name}.{:03}", parts.len());
        let mut writer = File::create(out.join(&part_name))?;
        let mut tee = Tee {
            inner: &mut reader,
            hasher: &mut whole,
        };
        let (size, sha256) = copy_hashing(&mut tee, &mut writer, max_part)?;
        if size == 0 && !parts.is_empty() {
            fs::remove_file(out.join(&part_name))?;
            break;
        }
        parts.push(Asset {
            name: part_name,
            size,
            sha256,
        });
        if size < max_part {
            break;
        }
    }
    let size = parts.iter().map(|part| part.size).sum();
    Ok(Rootfs {
        size,
        sha256: format!("{:x}", whole.finalize()),
        parts,
    })
}

/// Removes the `<name>.NNN` parts of an earlier release, so a shorter root filesystem leaves none behind to be
/// published with the new ones.
fn remove_old_parts(out: &Path, name: &str) -> io::Result<()> {
    for entry in fs::read_dir(out)? {
        let entry = entry?;
        let file = entry.file_name();
        let is_part = file
            .to_str()
            .and_then(|file| file.strip_prefix(name))
            .and_then(|rest| rest.strip_prefix('.'))
            .is_some_and(|digits| digits.len() >= 3 && digits.bytes().all(|b| b.is_ascii_digit()));
        if is_part {
            fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

struct Tee<'a, R: Read> {
    inner: &'a mut R,
    hasher: &'a mut Sha256,
}

impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..read]);
        Ok(read)
    }
}

/// Copies at most `limit` bytes; returns how many and their SHA-256.
fn copy_hashing(reader: &mut impl Read, writer: &mut impl Write, limit: u64) -> io::Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    let mut size = 0u64;
    while size < limit {
        let want = usize::try_from(limit - size)
            .unwrap_or(usize::MAX)
            .min(buffer.len());
        let read = reader.read(&mut buffer[..want])?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        writer.write_all(&buffer[..read])?;
        size += read as u64;
    }
    Ok((size, format!("{:x}", hasher.finalize())))
}

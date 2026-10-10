use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_image::{TRUSTED_PUBLIC_KEY, parse_signing_key, public_key_text};
use ed25519_dalek::SigningKey;

use crate::Result;

/// A fresh 32 byte seed from the system's random source.
pub fn random_seed() -> Result<[u8; 32]> {
    let mut seed = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut seed)?;
    Ok(seed)
}

/// Writes a new signing key to `file` (refusing to replace one) and prints the public key.
pub fn keygen(file: &str) -> Result<()> {
    let seed = random_seed()?;
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(file)?;
    writeln!(out, "{}", URL_SAFE_NO_PAD.encode(seed))?;
    println!("{}", public_key_text(&SigningKey::from_bytes(&seed)));
    Ok(())
}

/// The key a release is signed with: the trusted one from `key_file`, or with `ephemeral` one made for this
/// run. One of the two must be chosen, never both and never none.
pub fn release_key(key_file: Option<&Path>, ephemeral: bool) -> Result<SigningKey> {
    match (key_file, ephemeral) {
        (Some(file), false) => read_trusted_key(file),
        (None, true) => ephemeral_key(),
        _ => Err("give exactly one of --key-file <file> and --ephemeral".into()),
    }
}

/// Reads a signing key file and checks it is the key whose public half is committed as the trusted one.
fn read_trusted_key(file: &Path) -> Result<SigningKey> {
    let key = parse_signing_key(&std::fs::read_to_string(file)?)?;
    if public_key_text(&key) != TRUSTED_PUBLIC_KEY.trim() {
        return Err(
            "the key file is not the key whose public half is in crates/image/trusted-key.txt".into(),
        );
    }
    Ok(key)
}

/// Makes a signing key from a fresh seed, for builds that need no trust beyond themselves.
pub fn ephemeral_key() -> Result<SigningKey> {
    Ok(SigningKey::from_bytes(&random_seed()?))
}

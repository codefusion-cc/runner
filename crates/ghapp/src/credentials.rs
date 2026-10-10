use std::fmt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use cf_secret::Secret;
use ring::signature::RsaKeyPair;

/// Why the App's key or its files could not be used. The messages name files and shapes, never key material.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// A file could not be read.
    #[error("{path}: {why}")]
    Read {
        /// The file.
        path: PathBuf,
        /// What the system said.
        why: String,
    },
    /// The key file can be read by other users.
    #[error("{path} can be read by other users (mode {mode:o}); it must be 0600")]
    Permissions {
        /// The file.
        path: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
    /// `app.json` is not what it should be.
    #[error("{path}: {why}")]
    Config {
        /// The file.
        path: PathBuf,
        /// What was wrong.
        why: String,
    },
    /// The text is not a usable RSA private key.
    #[error("not a usable RSA private key in PEM form: {0}")]
    Invalid(&'static str),
}

/// The GitHub App's RSA private key. Its `Debug` shows nothing.
pub struct AppKey(RsaKeyPair);

impl AppKey {
    /// Reads a PEM-encoded RSA private key, either PKCS#1 (`BEGIN RSA PRIVATE KEY`, what GitHub hands out) or
    /// PKCS#8 (`BEGIN PRIVATE KEY`).
    pub fn from_pem(pem: &Secret<String>) -> Result<AppKey, KeyError> {
        let (label, der) = pem_block(pem.expose())?;
        let pair = match label {
            "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
            "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
            _ => return Err(KeyError::Invalid("not a private key block")),
        };
        pair.map(AppKey)
            .map_err(|_| KeyError::Invalid("the bytes are not an RSA key of 2048 bits or more"))
    }

    pub(crate) fn public_modulus_len(&self) -> usize {
        self.0.public().modulus_len()
    }

    pub(crate) fn sign(
        &self,
        padding: &'static dyn ring::signature::RsaEncoding,
        rng: &dyn ring::rand::SecureRandom,
        message: &[u8],
        out: &mut [u8],
    ) -> Result<(), ring::error::Unspecified> {
        self.0.sign(padding, rng, message, out)
    }
}

impl fmt::Debug for AppKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppKey(..)")
    }
}

/// The first PEM block of the text: its label and its decoded bytes.
fn pem_block(text: &str) -> Result<(&str, Vec<u8>), KeyError> {
    let start = text
        .find("-----BEGIN ")
        .ok_or(KeyError::Invalid("no PEM block"))?;
    let rest = &text[start + "-----BEGIN ".len()..];
    let label_end = rest
        .find("-----")
        .ok_or(KeyError::Invalid("the PEM header is cut off"))?;
    let label = &rest[..label_end];
    let body_start = &rest[label_end + 5..];
    let end_marker = format!("-----END {label}-----");
    let body_end = body_start
        .find(&end_marker)
        .ok_or(KeyError::Invalid("the PEM block has no end"))?;
    let body: String = body_start[..body_end]
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let der = STANDARD
        .decode(body)
        .map_err(|_| KeyError::Invalid("the PEM body is not base64"))?;
    Ok((label, der))
}

/// The App the Runner signs in as: its client ID and its private key.
#[derive(Debug)]
pub struct AppCredentials {
    /// The App's client ID, the `iss` of its tokens.
    pub client_id: String,
    /// The private key.
    pub key: AppKey,
}

impl AppCredentials {
    /// Reads `app.json` (`{"client_id": "..."}`) and `app.pem` from a folder. The key file must be readable by its
    /// owner alone (0600): a key other users can read is refused, not used.
    pub fn load(dir: &Path) -> Result<AppCredentials, KeyError> {
        let json_path = dir.join("app.json");
        let json = std::fs::read(&json_path).map_err(|e| KeyError::Read {
            path: json_path.clone(),
            why: e.kind().to_string(),
        })?;
        let value: serde_json::Value = serde_json::from_slice(&json).map_err(|_| KeyError::Config {
            path: json_path.clone(),
            why: "not JSON".into(),
        })?;
        let client_id = value["client_id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| KeyError::Config {
                path: json_path.clone(),
                why: "no client_id".into(),
            })?;

        let pem_path = dir.join("app.pem");
        check_mode(&pem_path)?;
        let pem = std::fs::read_to_string(&pem_path).map_err(|e| KeyError::Read {
            path: pem_path.clone(),
            why: e.kind().to_string(),
        })?;
        Ok(AppCredentials {
            client_id: client_id.to_string(),
            key: AppKey::from_pem(&Secret::new(pem))?,
        })
    }
}

#[cfg(unix)]
fn check_mode(path: &Path) -> Result<(), KeyError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = std::fs::metadata(path).map_err(|e| KeyError::Read {
        path: path.to_path_buf(),
        why: e.kind().to_string(),
    })?;
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(KeyError::Permissions {
            path: path.to_path_buf(),
            mode,
        });
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_mode(_: &Path) -> Result<(), KeyError> {
    Ok(())
}

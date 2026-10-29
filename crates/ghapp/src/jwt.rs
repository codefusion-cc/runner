use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cf_secret::Secret;
use ring::rand::SystemRandom;
use ring::signature::RSA_PKCS1_SHA256;

use crate::clock::Clock;
use crate::credentials::AppKey;
use crate::error::Error;

/// Signs the App's own tokens. GitHub refuses one issued in its future or valid for over ten minutes, so the token
/// is issued a minute in the past and valid for nine minutes; and the signer learns GitHub's time from its
/// responses (see [`Signer::learn_server_time`]), so a machine whose clock is off still gets a valid token.
pub(crate) struct Signer {
    client_id: String,
    key: AppKey,
    clock: Arc<dyn Clock>,
    /// GitHub's time minus ours, in seconds, as last learned.
    skew: AtomicI64,
}

impl Signer {
    pub(crate) fn new(client_id: String, key: AppKey, clock: Arc<dyn Clock>) -> Signer {
        Signer {
            client_id,
            key,
            clock,
            skew: AtomicI64::new(0),
        }
    }

    /// This machine's time, in seconds since the epoch.
    fn ours(&self) -> i64 {
        self.clock
            .now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_secs() as i64
    }

    /// Our idea of GitHub's current time, in seconds since the epoch.
    pub(crate) fn now_epoch(&self) -> i64 {
        self.ours() + self.skew.load(Ordering::Relaxed)
    }

    /// The offset between GitHub's time and ours, in seconds.
    pub(crate) fn skew(&self) -> i64 {
        self.skew.load(Ordering::Relaxed)
    }

    /// Takes the time from a response's `Date` header. Returns whether the offset changed by more than a few
    /// seconds, which tells the caller that a token refused for its clock is worth signing again.
    pub(crate) fn learn_server_time(&self, server_epoch: i64) -> bool {
        let new = server_epoch - self.ours();
        let old = self.skew.swap(new, Ordering::Relaxed);
        (new - old).abs() > 5
    }

    /// The signed token.
    pub(crate) fn token(&self) -> Result<Secret<String>, Error> {
        let now = self.now_epoch();
        let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = serde_json::json!({"iss": self.client_id, "iat": now - 60, "exp": now + 9 * 60});
        let claims = URL_SAFE_NO_PAD.encode(claims.to_string());
        let input = format!("{header}.{claims}");
        let mut signature = vec![0u8; self.key.public_modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &SystemRandom::new(),
                input.as_bytes(),
                &mut signature,
            )
            .map_err(|_| Error::Key("signing failed".into()))?;
        Ok(Secret::new(format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(signature)
        )))
    }
}

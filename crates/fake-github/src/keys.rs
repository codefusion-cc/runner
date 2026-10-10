use std::sync::OnceLock;

use rsa::RsaPrivateKey;
use rsa::pkcs1::{EncodeRsaPrivateKey, EncodeRsaPublicKey};

/// A throwaway RSA key pair for a test App. It signs nothing real and exists only inside the test process.
#[derive(Clone)]
pub struct TestKey {
    /// The private half as PKCS#1 PEM, the format GitHub hands out.
    pub private_pem: String,
    /// The public half as PKCS#1 DER, what the fake checks tokens with.
    pub public_der: Vec<u8>,
}

impl std::fmt::Debug for TestKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TestKey(..)")
    }
}

/// The test key, made once per process (generating one takes a moment).
pub fn test_key() -> &'static TestKey {
    static KEY: OnceLock<TestKey> = OnceLock::new();
    KEY.get_or_init(|| {
        let mut rng = rsa::rand_core::OsRng;
        let private = RsaPrivateKey::new(&mut rng, 2048).expect("generate a test key");
        let pem = private.to_pkcs1_pem(rsa::pkcs1::LineEnding::LF).expect("pem");
        let public_der = private
            .to_public_key()
            .to_pkcs1_der()
            .expect("der")
            .as_bytes()
            .to_vec();
        TestKey {
            private_pem: pem.to_string(),
            public_der,
        }
    })
}

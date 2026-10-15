//! Frame encryption: key = Argon2id(passphrase, fixed salt); frame = nonce || ChaCha20-Poly1305.

use anyhow::{Result, anyhow};
use chacha20poly1305::aead::{Aead, Generate, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};

pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 16;

/// App-specific constant salt. Every peer must derive the same key from the same
/// passphrase without a prior exchange, so the salt cannot be random.
const SALT: &[u8] = b"ClipBridge/v1/argon2id-salt";

#[derive(Clone)]
pub struct Cipher {
    aead: ChaCha20Poly1305,
}

#[derive(Debug)]
pub struct DecryptError;

impl std::fmt::Display for DecryptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("frame failed authentication (wrong passphrase or corrupted data)")
    }
}

impl std::error::Error for DecryptError {}

impl Cipher {
    /// Derives the key. Deliberately slow (Argon2id defaults: 19 MiB, 2 passes):
    /// call once at startup, off the async runtime.
    pub fn from_passphrase(passphrase: &str) -> Result<Self> {
        let mut key = [0u8; 32];
        argon2::Argon2::default()
            .hash_password_into(passphrase.as_bytes(), SALT, &mut key)
            .map_err(|e| anyhow!("key derivation failed: {e}"))?;
        Ok(Self {
            aead: ChaCha20Poly1305::new(&Key::from(key)),
        })
    }

    /// Returns `nonce || ciphertext+tag` with a fresh random nonce.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let nonce = Nonce::generate();
        let ct = self
            .aead
            .encrypt(&nonce, plaintext)
            .map_err(|_| anyhow!("encryption failed"))?;
        let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
        out.extend_from_slice(nonce.as_slice());
        out.extend_from_slice(&ct);
        Ok(out)
    }

    pub fn open(&self, frame: &[u8]) -> Result<Vec<u8>, DecryptError> {
        if frame.len() < NONCE_LEN + TAG_LEN {
            return Err(DecryptError);
        }
        let (nonce, ct) = frame.split_at(NONCE_LEN);
        let nonce = Nonce::try_from(nonce).map_err(|_| DecryptError)?;
        self.aead.decrypt(&nonce, ct).map_err(|_| DecryptError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_fresh_nonces() {
        let c = Cipher::from_passphrase("correct horse").unwrap();
        let a = c.seal(b"hello").unwrap();
        let b = c.seal(b"hello").unwrap();
        assert_ne!(a, b, "nonce must be random per frame");
        assert_eq!(a.len(), NONCE_LEN + 5 + TAG_LEN);
        assert_eq!(c.open(&a).unwrap(), b"hello");
        assert_eq!(c.open(&c.seal(b"").unwrap()).unwrap(), b"");
    }

    #[test]
    fn same_passphrase_same_key() {
        let a = Cipher::from_passphrase("shared").unwrap();
        let b = Cipher::from_passphrase("shared").unwrap();
        assert_eq!(b.open(&a.seal(b"x").unwrap()).unwrap(), b"x");
    }

    #[test]
    fn tamper_and_wrong_key_detected() {
        let c = Cipher::from_passphrase("one").unwrap();
        let frame = c.seal(b"secret data").unwrap();
        for i in [0, NONCE_LEN, frame.len() - 1] {
            let mut bad = frame.clone();
            bad[i] ^= 0x01;
            assert!(c.open(&bad).is_err(), "flip at {i} must fail");
        }
        assert!(c.open(&frame[..frame.len() - 1]).is_err());
        assert!(c.open(&[0; 10]).is_err());
        let other = Cipher::from_passphrase("two").unwrap();
        assert!(other.open(&frame).is_err());
    }
}

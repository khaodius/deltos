//! The Ed25519 key pair that signs update manifests.
//!
//! The private half lives only in the `DELTOS_UPDATE_KEY` secret of the release workflow;
//! the public half is committed as `packaging/update-key.pub` and compiled into the app. Both
//! are base64 text: the 32-byte seed for the private key, the 32-byte point for the public
//! key, and a signature is the 64-byte Ed25519 signature in base64. Verification is strict,
//! so a signature from a small-order or non-canonical key is rejected as well.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};

use crate::Error;

/// Why a key could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyError {
    #[error("the key is empty")]
    Empty,
    #[error("the key is not base64")]
    Encoding,
    #[error("the key is {0} bytes, not 32")]
    Length(usize),
    #[error("the key is not a valid Ed25519 key")]
    Invalid,
    #[error("the system gave no random bytes for a new key")]
    Random,
}

fn decode32(text: &str) -> Result<[u8; 32], KeyError> {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.is_empty() {
        return Err(KeyError::Empty);
    }
    let bytes = BASE64.decode(compact).map_err(|_| KeyError::Encoding)?;
    <[u8; 32]>::try_from(bytes.as_slice()).map_err(|_| KeyError::Length(bytes.len()))
}

/// The public half of the update key, which the app embeds.
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey {
    key: VerifyingKey,
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({})", self.to_base64())
    }
}

impl PublicKey {
    /// Reads a key from its base64 text, ignoring surrounding whitespace and line breaks.
    pub fn parse(text: &str) -> Result<PublicKey, KeyError> {
        let bytes = decode32(text)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| KeyError::Invalid)?;
        if key.is_weak() {
            return Err(KeyError::Invalid);
        }
        Ok(PublicKey { key })
    }

    pub fn to_base64(&self) -> String {
        BASE64.encode(self.key.as_bytes())
    }

    /// Checks that `signature` (base64) was made over exactly `message` by the private half of
    /// this key.
    pub fn verify(&self, message: &[u8], signature: &str) -> Result<(), Error> {
        let compact: String = signature.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.is_empty() {
            return Err(Error::Signature("the signature is empty".into()));
        }
        let bytes = BASE64
            .decode(compact)
            .map_err(|_| Error::Signature("the signature is not base64".into()))?;
        let signature = Signature::from_slice(&bytes).map_err(|_| {
            Error::Signature(format!(
                "the signature is {} bytes, not {}",
                bytes.len(),
                ed25519_dalek::SIGNATURE_LENGTH
            ))
        })?;
        self.key
            .verify_strict(message, &signature)
            .map_err(|_| Error::Signature("the signature does not match the file".into()))
    }
}

/// The private half of the update key. Only `deltos-sign` ever holds one.
#[derive(Clone)]
pub struct SecretKey {
    key: SigningKey,
}

impl fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the key itself, not even in logs.
        f.write_str("SecretKey(..)")
    }
}

impl SecretKey {
    /// A new random key from the operating system's randomness.
    pub fn generate() -> Result<SecretKey, KeyError> {
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|_| KeyError::Random)?;
        Ok(SecretKey::from_seed(seed))
    }

    pub fn from_seed(seed: [u8; 32]) -> SecretKey {
        SecretKey {
            key: SigningKey::from_bytes(&seed),
        }
    }

    pub fn parse(text: &str) -> Result<SecretKey, KeyError> {
        decode32(text).map(SecretKey::from_seed)
    }

    pub fn to_base64(&self) -> String {
        BASE64.encode(self.key.as_bytes())
    }

    pub fn public_key(&self) -> PublicKey {
        PublicKey {
            key: self.key.verifying_key(),
        }
    }

    /// The base64 signature over `message`.
    pub fn sign(&self, message: &[u8]) -> String {
        BASE64.encode(self.key.sign(message).to_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key() -> SecretKey {
        SecretKey::from_seed([7; 32])
    }

    #[test]
    fn signs_and_verifies() {
        let secret = key();
        let signature = secret.sign(b"manifest bytes");
        assert!(
            secret
                .public_key()
                .verify(b"manifest bytes", &signature)
                .is_ok()
        );
        // Whitespace around the text, as a file with a trailing newline has, is fine.
        assert!(
            secret
                .public_key()
                .verify(b"manifest bytes", &format!("{signature}\n"))
                .is_ok()
        );
    }

    #[test]
    fn rejects_a_changed_message() {
        let secret = key();
        let signature = secret.sign(b"manifest bytes");
        assert!(matches!(
            secret.public_key().verify(b"manifest byteS", &signature),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn rejects_a_changed_signature() {
        let secret = key();
        let signature = secret.sign(b"manifest bytes");
        let mut bytes = BASE64.decode(&signature).unwrap();
        bytes[10] ^= 0x01;
        let tampered = BASE64.encode(bytes);
        assert!(matches!(
            secret.public_key().verify(b"manifest bytes", &tampered),
            Err(Error::Signature(_))
        ));
        assert!(matches!(
            secret.public_key().verify(b"manifest bytes", "AAAA"),
            Err(Error::Signature(_))
        ));
        assert!(matches!(
            secret.public_key().verify(b"manifest bytes", ""),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn rejects_another_key() {
        let signature = key().sign(b"manifest bytes");
        let other = SecretKey::from_seed([8; 32]).public_key();
        assert!(matches!(
            other.verify(b"manifest bytes", &signature),
            Err(Error::Signature(_))
        ));
    }

    #[test]
    fn keys_round_trip_through_base64() {
        let secret = key();
        let parsed = SecretKey::parse(&format!("  {}\n", secret.to_base64())).unwrap();
        assert_eq!(parsed.to_base64(), secret.to_base64());
        let public = PublicKey::parse(&secret.public_key().to_base64()).unwrap();
        assert_eq!(public, secret.public_key());
    }

    #[test]
    fn refuses_bad_keys() {
        assert_eq!(PublicKey::parse(""), Err(KeyError::Empty));
        assert_eq!(PublicKey::parse(" \n"), Err(KeyError::Empty));
        assert_eq!(PublicKey::parse("not base64!"), Err(KeyError::Encoding));
        assert_eq!(
            PublicKey::parse(&BASE64.encode([1u8; 16])),
            Err(KeyError::Length(16))
        );
        // The identity point is a valid encoding but a weak key.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        assert_eq!(
            PublicKey::parse(&BASE64.encode(identity)),
            Err(KeyError::Invalid)
        );
        assert!(matches!(SecretKey::parse(""), Err(KeyError::Empty)));
    }

    #[test]
    fn generates_distinct_keys() {
        let a = SecretKey::generate().unwrap();
        let b = SecretKey::generate().unwrap();
        assert_ne!(a.to_base64(), b.to_base64());
        assert_eq!(format!("{a:?}"), "SecretKey(..)");
    }
}

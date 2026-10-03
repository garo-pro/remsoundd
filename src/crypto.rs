//! Key derivation and the sealed-payload envelope. See docs/PROTOCOL.md, "Crypto".
//!
//! Every value here is part of the cross-port contract: a different salt, hash, length or
//! iteration count derives a different key from the same password, and the far end hears
//! nothing at all rather than an error.

use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce, Tag};
use rand::RngCore;

pub const KEY_BYTES: usize = 32;
pub const FINGERPRINT_BYTES: usize = 8;
pub const NONCE_BYTES: usize = 12;
pub const TAG_BYTES: usize = 16;
/// Bytes a seal adds on top of the plaintext: nonce plus tag.
pub const OVERHEAD_BYTES: usize = NONCE_BYTES + TAG_BYTES;

/// MUST stay 100,000. v5.6 raised it to 600k and the iOS app went silent.
pub const PBKDF2_ITERATIONS: u32 = 100_000;
const KEY_SALT: &[u8] = b"RemSound.v1.audio-key";
const FINGERPRINT_SALT: &[u8] = b"RemSound.v1.fingerprint";

pub type Key = [u8; KEY_BYTES];
pub type Fingerprint = [u8; FINGERPRINT_BYTES];

pub fn derive_key(password: &str) -> Key {
    let mut key = [0u8; KEY_BYTES];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), KEY_SALT, PBKDF2_ITERATIONS, &mut key);
    key
}

pub fn fingerprint(password: &str) -> Fingerprint {
    let mut print = [0u8; FINGERPRINT_BYTES];
    pbkdf2::pbkdf2_hmac::<sha2::Sha256>(password.as_bytes(), FINGERPRINT_SALT, PBKDF2_ITERATIONS, &mut print);
    print
}

/// Constant-time comparison, so timing never says how much of a fingerprint matched.
pub fn fingerprints_equal(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// The password's derived credentials, computed once.
#[derive(Clone)]
pub struct Credentials {
    pub key: Key,
    pub fingerprint: Fingerprint,
}

impl Credentials {
    pub fn from_password(password: &str) -> Self {
        Self { key: derive_key(password), fingerprint: fingerprint(password) }
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the key.
        f.debug_struct("Credentials").field("fingerprint", &hex(&self.fingerprint)).finish_non_exhaustive()
    }
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// A reusable AES-256-GCM cipher for one key.
#[derive(Clone)]
pub struct Cipher {
    aead: Aes256Gcm,
}

impl Cipher {
    pub fn new(key: &Key) -> Self {
        Self { aead: Aes256Gcm::new(key.into()) }
    }

    /// Seal with an explicit nonce: `nonce(12) || tag(16) || ciphertext`.
    pub fn seal_with_nonce(&self, nonce: &[u8; NONCE_BYTES], plaintext: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(OVERHEAD_BYTES + plaintext.len());
        out.extend_from_slice(nonce);
        out.extend_from_slice(&[0u8; TAG_BYTES]);
        out.extend_from_slice(plaintext);
        let (head, body) = out.split_at_mut(OVERHEAD_BYTES);
        let tag = self
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(nonce), &[], body)
            .expect("AES-GCM encryption cannot fail for in-range lengths");
        head[NONCE_BYTES..].copy_from_slice(&tag);
        out
    }

    /// Seal with a fresh random nonce, as tick proofs and control commands do.
    pub fn seal_random(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce = [0u8; NONCE_BYTES];
        rand::thread_rng().fill_bytes(&mut nonce);
        self.seal_with_nonce(&nonce, plaintext)
    }

    /// Seal with the next nonce from a counter sequence, as the audio path does.
    pub fn seal_next(&self, nonces: &mut NonceSequence, plaintext: &[u8]) -> Vec<u8> {
        let nonce = nonces.next_nonce();
        self.seal_with_nonce(&nonce, plaintext)
    }

    /// Open a sealed payload. None if it is too short, or the tag fails (another password, or tampering).
    pub fn open(&self, sealed: &[u8]) -> Option<Vec<u8>> {
        if sealed.len() < OVERHEAD_BYTES {
            return None;
        }
        let nonce = Nonce::from_slice(&sealed[..NONCE_BYTES]);
        let tag = Tag::from_slice(&sealed[NONCE_BYTES..OVERHEAD_BYTES]);
        let mut plain = sealed[OVERHEAD_BYTES..].to_vec();
        self.aead.decrypt_in_place_detached(nonce, &[], &mut plain, tag).ok()?;
        Some(plain)
    }
}

/// The audio nonce generator: a random 48-bit prefix per instance, then a 48-bit little-endian
/// counter. Uniqueness within an instance is arithmetic; across instances (every restart, under
/// the same long-lived key) the random prefix keeps the counter ranges apart. Never reuse one.
pub struct NonceSequence {
    prefix: [u8; 6],
    counter: u64,
}

impl NonceSequence {
    pub fn new() -> Self {
        let mut prefix = [0u8; 6];
        rand::thread_rng().fill_bytes(&mut prefix);
        Self { prefix, counter: 0 }
    }

    #[cfg(test)]
    pub fn with_prefix(prefix: [u8; 6], counter: u64) -> Self {
        Self { prefix, counter }
    }

    pub fn next_nonce(&mut self) -> [u8; NONCE_BYTES] {
        let mut nonce = [0u8; NONCE_BYTES];
        nonce[..6].copy_from_slice(&self.prefix);
        let c = self.counter;
        self.counter = (self.counter + 1) & 0xFFFF_FFFF_FFFF;
        nonce[6..].copy_from_slice(&c.to_le_bytes()[..6]);
        nonce
    }
}

impl Default for NonceSequence {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn counter_nonces_are_prefix_then_little_endian_counter() {
        let mut seq = NonceSequence::with_prefix([1, 2, 3, 4, 5, 6], 0x0102);
        assert_eq!(seq.next_nonce(), [1, 2, 3, 4, 5, 6, 0x02, 0x01, 0, 0, 0, 0]);
        assert_eq!(seq.next_nonce(), [1, 2, 3, 4, 5, 6, 0x03, 0x01, 0, 0, 0, 0]);
    }

    #[test]
    fn nonces_never_repeat_within_or_across_instances() {
        let mut seen = HashSet::new();
        for _ in 0..64 {
            let mut seq = NonceSequence::new();
            for _ in 0..256 {
                assert!(seen.insert(seq.next_nonce()), "a nonce repeated");
            }
        }
    }

    #[test]
    fn seal_and_open_round_trip_and_wrong_key_fails() {
        let a = Cipher::new(&[7u8; 32]);
        let b = Cipher::new(&[8u8; 32]);
        let sealed = a.seal_random(b"hello");
        assert_eq!(sealed.len(), 5 + OVERHEAD_BYTES);
        assert_eq!(a.open(&sealed).as_deref(), Some(&b"hello"[..]));
        assert!(b.open(&sealed).is_none());
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(a.open(&tampered).is_none());
        assert!(a.open(&sealed[..27]).is_none());
    }
}

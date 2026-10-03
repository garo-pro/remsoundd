//! TickProof, packet type 11: "I have ticked you, and I hold the password".
//!
//! Sealed payload of 53 bytes: a random-nonce AES-GCM seal over version (1), Unix seconds (i64 LE)
//! and the sender's instance GUID in RFC 4122 big-endian order.

use std::collections::HashMap;
use std::time::Duration;

use uuid::Uuid;

use crate::crypto::{Cipher, OVERHEAD_BYTES};

pub const FORMAT_VERSION: u8 = 1;
pub const PLAIN_BYTES: usize = 1 + 8 + 16;
pub const SEALED_PAYLOAD_BYTES: usize = PLAIN_BYTES + OVERHEAD_BYTES; // 53
pub const SEND_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_MAX_SKEW_SECS: i64 = 10 * 60;
const MAX_REMEMBERED: usize = 4096;

pub fn plaintext(instance: Uuid, unix_secs: i64) -> [u8; PLAIN_BYTES] {
    let mut plain = [0u8; PLAIN_BYTES];
    plain[0] = FORMAT_VERSION;
    plain[1..9].copy_from_slice(&unix_secs.to_le_bytes());
    plain[9..].copy_from_slice(instance.as_bytes());
    plain
}

pub fn seal(cipher: &Cipher, instance: Uuid, unix_secs: i64) -> Vec<u8> {
    cipher.seal_random(&plaintext(instance, unix_secs))
}

/// What an opened proof says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Proof {
    pub instance: Uuid,
    pub unix_secs: i64,
    /// The first 8 nonce bytes, little-endian: the replay memory's key.
    pub nonce_id: u64,
}

/// Open a proof. None when the size is wrong, the seal does not open, or the version is unknown.
pub fn unseal(cipher: &Cipher, payload: &[u8]) -> Option<Proof> {
    if payload.len() != SEALED_PAYLOAD_BYTES {
        return None;
    }
    let plain = cipher.open(payload)?;
    if plain.len() != PLAIN_BYTES || plain[0] != FORMAT_VERSION {
        return None;
    }
    Some(Proof {
        unix_secs: i64::from_le_bytes(plain[1..9].try_into().unwrap()),
        instance: Uuid::from_bytes(plain[9..25].try_into().unwrap()),
        nonce_id: u64::from_le_bytes(payload[..8].try_into().unwrap()),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    NotOurPassword,
    Stale { skew_secs: i64 },
    Replay,
}

impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotOurPassword => write!(f, "not our password"),
            Self::Stale { skew_secs } => write!(
                f,
                "stale, its clock is {} minutes away from ours",
                skew_secs.abs() / 60
            ),
            Self::Replay => write!(f, "a replay of a proof already used"),
        }
    }
}

/// Receiver-side gate, as TickProofGuard: authenticate, bound clock skew, never accept a seal twice.
pub struct Guard {
    max_skew_secs: i64,
    seen: HashMap<u64, i64>,
}

impl Guard {
    pub fn new(max_skew_secs: i64) -> Self {
        Self {
            max_skew_secs,
            seen: HashMap::new(),
        }
    }

    pub fn accept(
        &mut self,
        cipher: &Cipher,
        payload: &[u8],
        now_unix_secs: i64,
    ) -> Result<Proof, Rejection> {
        let proof = unseal(cipher, payload).ok_or(Rejection::NotOurPassword)?;
        let skew = now_unix_secs - proof.unix_secs;
        if skew.abs() > self.max_skew_secs {
            return Err(Rejection::Stale { skew_secs: skew });
        }
        if self.seen.contains_key(&proof.nonce_id) {
            return Err(Rejection::Replay);
        }
        if self.seen.len() >= MAX_REMEMBERED {
            let window = 2 * self.max_skew_secs;
            self.seen.retain(|_, at| now_unix_secs - *at <= window);
            if self.seen.len() >= MAX_REMEMBERED {
                self.seen.clear();
            }
        }
        self.seen.insert(proof.nonce_id, now_unix_secs);
        Ok(proof)
    }
}

impl Default for Guard {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SKEW_SECS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher(byte: u8) -> Cipher {
        Cipher::new(&[byte; 32])
    }

    #[test]
    fn seal_and_open() {
        let id = Uuid::new_v4();
        let sealed = seal(&cipher(1), id, 1_790_000_000);
        assert_eq!(sealed.len(), SEALED_PAYLOAD_BYTES);
        let proof = unseal(&cipher(1), &sealed).unwrap();
        assert_eq!((proof.instance, proof.unix_secs), (id, 1_790_000_000));
        assert!(unseal(&cipher(2), &sealed).is_none());
        assert!(unseal(&cipher(1), &sealed[..52]).is_none());
    }

    #[test]
    fn guard_rejects_wrong_password_skew_and_replay() {
        let c = cipher(1);
        let mut guard = Guard::default();
        let now = 1_790_000_000;
        let fresh = seal(&c, Uuid::new_v4(), now - 30);
        assert!(guard.accept(&c, &fresh, now).is_ok());
        assert_eq!(guard.accept(&c, &fresh, now), Err(Rejection::Replay));
        assert_eq!(
            guard.accept(&cipher(2), &seal(&c, Uuid::new_v4(), now), now),
            Err(Rejection::NotOurPassword)
        );
        assert!(matches!(
            guard.accept(&c, &seal(&c, Uuid::new_v4(), now - 601), now),
            Err(Rejection::Stale { .. })
        ));
        assert!(matches!(
            guard.accept(&c, &seal(&c, Uuid::new_v4(), now + 601), now),
            Err(Rejection::Stale { .. })
        ));
        assert!(guard
            .accept(&c, &seal(&c, Uuid::new_v4(), now + 599), now)
            .is_ok());
    }

    #[test]
    fn wrong_version_is_refused() {
        let c = cipher(1);
        let mut plain = plaintext(Uuid::new_v4(), 5);
        plain[0] = 2;
        assert!(unseal(&c, &c.seal_random(&plain)).is_none());
    }
}

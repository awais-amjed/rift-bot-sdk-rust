//! Opening what a member sealed for this bot, and sealing a DM back.
//!
//! A bot never holds a channel key it was not granted, so this is small: its
//! own X25519 identity, `wrap:v1` to open a key sealed to it (a call's media
//! key, a granted channel's keyring), and AES-256-GCM the way the Dart side
//! writes it, with the 16-byte tag on the end of the ciphertext.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use serde::Deserialize;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::crypto::{b64, hmac, random_bytes, unb64};

/// This bot's X25519 identity on one host: what members seal things to.
pub struct ChatIdentity {
    secret: StaticSecret,
    /// 32 raw bytes: what the server stores as `users.chat_public_key`.
    pub public_key: [u8; 32],
}

impl std::fmt::Debug for ChatIdentity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChatIdentity")
            .field("public_key", &b64(&self.public_key))
            .finish_non_exhaustive()
    }
}

/// Per host and pinned at `v1`, deliberately not scoped to a server and not
/// following the sign-in key's version (WIRE.md §2). Rotating it would make
/// everything ever sealed to the old one unopenable.
pub fn derive_chat_identity(seed: &[u8], host: &str) -> ChatIdentity {
    // RFC 7748 clamping happens inside x25519-dalek, on use.
    let secret = StaticSecret::from(hmac(seed, &format!("{host}:chat:v1")));
    ChatIdentity {
        public_key: PublicKey::from(&secret).to_bytes(),
        secret,
    }
}

impl ChatIdentity {
    fn shared_with(&self, their_public: &[u8]) -> Option<[u8; 32]> {
        let theirs = <[u8; 32]>::try_from(their_public).ok()?;
        Some(
            self.secret
                .diffie_hellman(&PublicKey::from(theirs))
                .to_bytes(),
        )
    }
}

/// One sealed key, as it comes off the row.
#[derive(Debug, Clone, Deserialize)]
pub struct Wrapped {
    pub ephemeral_public_key: String,
    pub ciphertext: String,
    pub nonce: String,
}

/// Open a key sealed to `identity` (`wrap:v1`). `None` if anything is off,
/// the tag included.
///
/// The plaintext inside the envelope is the key's **base64**, not its bytes:
/// the Dart side seals a string. A port that decoded once would get 44 bytes
/// of ASCII where it wanted 32, and fail at the frame cryptor, far from here.
pub fn unwrap_key(identity: &ChatIdentity, wrapped: &Wrapped) -> Option<Vec<u8>> {
    let shared = identity.shared_with(&unb64(&wrapped.ephemeral_public_key)?)?;
    let wrapping_key = hmac(&shared, "wrap:v1");
    let plaintext = open_bytes(&wrapped.ciphertext, &wrapped.nonce, &wrapping_key)?;
    unb64(std::str::from_utf8(&plaintext).ok()?)
}

/// The conversation key shared with one peer, from their chat public key
/// (base64). Pure ECDH: never stored, never sent.
pub fn derive_dm_key(identity: &ChatIdentity, peer_chat_public_key: &str) -> Option<[u8; 32]> {
    let shared = identity.shared_with(&unb64(peer_chat_public_key)?)?;
    Some(hmac(&shared, "dm:v1"))
}

/// Open an AES-256-GCM body (base64 ciphertext with the tag on, base64 nonce)
/// as text.
pub fn open(ciphertext: &str, nonce: &str, key: &[u8]) -> Option<String> {
    String::from_utf8(open_bytes(ciphertext, nonce, key)?).ok()
}

fn open_bytes(ciphertext: &str, nonce: &str, key: &[u8]) -> Option<Vec<u8>> {
    let cipher = Aes256Gcm::new_from_slice(key).ok()?;
    let nonce = unb64(nonce)?;
    if nonce.len() != 12 {
        return None;
    }
    cipher
        .decrypt(
            &Nonce::try_from(nonce.as_slice()).ok()?,
            unb64(ciphertext)?.as_slice(),
        )
        .ok()
}

/// Seal text the way the Dart side does: a fresh 12-byte nonce, the tag on the
/// end. Returns `(ciphertext, nonce)`, both base64.
pub fn seal(plaintext: &str, key: &[u8; 32]) -> (String, String) {
    let cipher = Aes256Gcm::new_from_slice(key).expect("a 32-byte key");
    let nonce = random_bytes::<12>();
    let sealed = cipher
        .encrypt(&Nonce::from(nonce), plaintext.as_bytes())
        .expect("AES-GCM seals any length a message has");
    (b64(&sealed), b64(&nonce))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_then_open() {
        let key = [3u8; 32];
        let (ciphertext, nonce) = seal("hello", &key);
        assert_eq!(open(&ciphertext, &nonce, &key).as_deref(), Some("hello"));
        assert_eq!(open(&ciphertext, &nonce, &[4u8; 32]), None);
    }
}

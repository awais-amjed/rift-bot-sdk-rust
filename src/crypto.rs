//! The key ladder and the signing format, exactly as `WIRE.md` describes them.
//!
//! Every value this module produces is checked against
//! `tests/wire_vectors.json`, the same file the Dart implementation and the
//! TypeScript SDK are checked against. That is the only reason three
//! implementations can be trusted to agree: a payload that differs by one
//! character stores fine, verifies as false, and renders as nothing.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// HMAC-SHA256 over the UTF-8 bytes of `message`. Every rung of the ladder.
pub fn hmac(key: &[u8], message: &str) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes a key of any length");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().into()
}

pub(crate) fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub(crate) fn unb64(text: &str) -> Option<Vec<u8>> {
    STANDARD.decode(text).ok()
}

/// The identity a bot signs in and signs messages with, on one server.
pub struct ServerIdentity {
    signing_key: SigningKey,
    /// 32 raw bytes: what the server stores as `users.public_key`.
    pub public_key: [u8; 32],
    /// base64, and the thing a ban is remembered by.
    pub stable_id: String,
}

impl std::fmt::Debug for ServerIdentity {
    // The signing key is the bot. It stays out of every log line.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerIdentity")
            .field("public_key", &b64(&self.public_key))
            .finish_non_exhaustive()
    }
}

/// The identity a bot logs in with.
///
/// Scoped to `(host, serverId)`, so one seed across N servers is N unrelated
/// bots and no server can correlate them. `host` is the URL's host **only**,
/// no scheme and no port, which is the mistake a port makes first. `None` for
/// `server_id` is the central host's scope.
pub fn derive_server_identity(seed: &[u8], host: &str, server_id: Option<&str>) -> ServerIdentity {
    let scope = match server_id {
        Some(id) => format!("{host}:{id}"),
        None => host.to_string(),
    };
    // A seed, not a scalar: RFC 8032, what `fromSeed` means in every library.
    let signing_key = SigningKey::from_bytes(&hmac(seed, &format!("{scope}:v1")));
    ServerIdentity {
        public_key: signing_key.verifying_key().to_bytes(),
        stable_id: b64(&hmac(seed, &format!("{scope}:identity"))),
        signing_key,
    }
}

impl ServerIdentity {
    /// Ed25519 over the UTF-8 bytes, base64 out. Deterministic, so testable.
    pub fn sign(&self, payload: &str) -> String {
        b64(&self.signing_key.sign(payload.as_bytes()).to_bytes())
    }
}

/// Check a signature against a raw 32-byte Ed25519 public key.
///
/// The half a *reader* does. A row whose signature does not check out is not
/// from who it claims, and acting on one is the whole attack, so anything
/// malformed is simply false.
pub fn verify_signature(payload: &str, signature_base64: &str, public_key: &[u8]) -> bool {
    let Ok(key_bytes) = <[u8; 32]>::try_from(public_key) else {
        return false;
    };
    let Ok(key) = VerifyingKey::from_bytes(&key_bytes) else {
        return false;
    };
    let Some(sig_bytes) = unb64(signature_base64) else {
        return false;
    };
    let Ok(sig) = Signature::from_slice(&sig_bytes) else {
        return false;
    };
    key.verify(payload.as_bytes(), &sig).is_ok()
}

/// The one string every message is signed over.
///
/// Joined with `:` and nothing else. A bot's message uses version 0 and an
/// empty nonce, which leaves two colons together: dropping the empty field
/// gives a payload one character shorter and a signature nothing verifies.
pub fn signed_payload(context_id: &str, key_version: i64, nonce: &str, ciphertext: &str) -> String {
    format!("chatmsg:v1:{context_id}:{key_version}:{nonce}:{ciphertext}")
}

/// Sorted, so both sides of a DM derive the same one without agreeing first.
pub fn conversation_context(a: &str, b: &str) -> String {
    let (first, second) = if a <= b { (a, b) } else { (b, a) };
    format!("dm:{first}:{second}")
}

const B58: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// Only the SIWS address is base58. Everything else on the wire is base64.
pub fn to_base58(bytes: &[u8]) -> String {
    // Repeated division of a big-endian number, one base-58 digit at a time.
    let mut digits: Vec<u8> = Vec::new();
    for &byte in bytes {
        let mut carry = byte as u32;
        for digit in digits.iter_mut() {
            carry += (*digit as u32) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    // A leading zero byte carries no magnitude, so it is counted, not computed.
    let zeros = bytes.iter().take_while(|&&b| b == 0).count();
    let mut out = "1".repeat(zeros);
    out.extend(digits.iter().rev().map(|&d| B58[d as usize] as char));
    out
}

/// The SIWS message GoTrue's web3 grant expects, and its signature.
///
/// `localhost` is a constant and not the server's address (WIRE.md §4):
/// GoTrue refuses most real addresses, and the two fields exist so a *wallet*
/// can say which site is asking. Rift has no wallet, and the key is derived
/// per `(host, serverId)` and posted only to the host it was derived for.
pub fn sign_siws(identity: &ServerIdentity) -> (String, String) {
    let address = to_base58(&identity.public_key);
    let nonce: String = b64(&random_bytes::<12>())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let message = format!(
        "localhost wants you to sign in with your Solana account:\n\
         {address}\n\
         \n\
         Sign in to Rift.\n\
         \n\
         URI: http://localhost\n\
         Version: 1\n\
         Chain ID: solana:mainnet\n\
         Nonce: {nonce}\n\
         Issued At: {}",
        iso_now()
    );
    let signature = identity.sign(&message);
    (message, signature)
}

pub(crate) fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    getrandom::fill(&mut out).expect("the OS has a random source");
    out
}

/// Now, as `Date.toISOString` writes it: `2026-10-13T09:04:05.123Z`.
fn iso_now() -> String {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    iso_at(since.as_millis() as i64)
}

fn iso_at(millis: i64) -> String {
    let secs = millis.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Days to a civil date, after Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        millis.rem_euclid(1000)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_matches_to_iso_string() {
        assert_eq!(iso_at(0), "1970-01-01T00:00:00.000Z");
        // new Date(1791881045123).toISOString()
        assert_eq!(iso_at(1_791_881_045_123), "2026-10-13T08:44:05.123Z");
        assert_eq!(iso_at(951_782_400_000), "2000-02-29T00:00:00.000Z");
    }

    #[test]
    fn base58_counts_leading_zeros() {
        assert_eq!(to_base58(&[]), "");
        assert_eq!(to_base58(&[0, 0, 1]), "112");
        assert_eq!(to_base58(b"hello world"), "StV1DL6CwTryKyV");
    }

    #[test]
    fn siws_has_the_fixed_template() {
        let identity = derive_server_identity(&[7; 32], "rift.example", Some("s"));
        let (message, signature) = sign_siws(&identity);
        let lines: Vec<&str> = message.lines().collect();
        assert_eq!(
            lines[0],
            "localhost wants you to sign in with your Solana account:"
        );
        assert_eq!(lines[1], to_base58(&identity.public_key));
        assert_eq!(lines[5], "URI: http://localhost");
        assert!(lines[8].starts_with("Nonce: "));
        assert!(lines[8][7..].chars().all(|c| c.is_ascii_alphanumeric()));
        assert!(verify_signature(&message, &signature, &identity.public_key));
    }
}

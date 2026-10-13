//! The whole reason `WIRE.md` and the vectors exist.
//!
//! A second implementation checking itself against numbers the first one
//! produced. Nothing here reads the Dart code, and nothing in the Dart code
//! reads this: they meet at one JSON file, `tests/wire_vectors.json`, which
//! the app's `tool/gen_wire_vectors.dart` writes along with this repository's
//! copy of `WIRE.md`.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rift_bot::body::{decode_body, encode_body};
use rift_bot::crypto::*;
use rift_bot::parse_invite;
use rift_bot::sealed::*;
use serde_json::Value;

fn vectors() -> Value {
    serde_json::from_str(include_str!("wire_vectors.json")).unwrap()
}

fn s<'a>(v: &'a Value, path: &str) -> &'a str {
    path.split('.').fold(v, |v, k| &v[k]).as_str().unwrap_or_else(|| panic!("{path}"))
}

fn seed(v: &Value) -> Vec<u8> {
    STANDARD.decode(s(v, "seed_base64")).unwrap()
}

fn b64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

fn server_identity(v: &Value) -> ServerIdentity {
    derive_server_identity(
        &seed(v),
        s(v, "server_identity.host"),
        Some(s(v, "server_identity.server_id")),
    )
}

fn wrapped(v: &Value) -> Wrapped {
    serde_json::from_value(v["wrapped_key"].clone()).unwrap()
}

#[test]
fn the_key_ladder_hmac_over_the_domain_strings() {
    let v = vectors();
    for (key, expected) in v["hmac"].as_object().unwrap() {
        let message = if key == "vault_key" { "vault:v1" } else { &key["context_".len()..] };
        assert_eq!(b64(&hmac(&seed(&v), message)), expected.as_str().unwrap(), "{message}");
    }
}

#[test]
fn a_server_identity_is_scoped_to_host_and_server() {
    let v = vectors();
    let identity = server_identity(&v);
    assert_eq!(b64(&identity.public_key), s(&v, "server_identity.public_key_base64"));
    assert_eq!(identity.stable_id, s(&v, "server_identity.stable_id"));
    assert_eq!(to_base58(&identity.public_key), s(&v, "server_identity.public_key_base58"));
}

#[test]
fn the_central_identity_has_no_server_in_its_scope() {
    let v = vectors();
    let identity = derive_server_identity(&seed(&v), s(&v, "central_identity.host"), None);
    assert_eq!(b64(&identity.public_key), s(&v, "central_identity.public_key_base64"));
    assert_eq!(identity.stable_id, s(&v, "central_identity.stable_id"));
}

#[test]
fn the_signed_payload_both_shapes() {
    let v = vectors();
    let channel = "aaaa1111-0000-4000-8000-000000000001";
    assert_eq!(signed_payload(channel, 3, "bm9uY2U=", "Y2lwaGVy"), s(&v, "signed_payload.channel"));
    // Version 0 and an empty nonce leave two colons together.
    let bot = signed_payload(channel, 0, "", "/echo hello");
    assert_eq!(bot, s(&v, "signed_payload.bot_plaintext"));
    assert!(bot.contains(":0::"));
}

#[test]
fn a_dm_context_is_sorted() {
    let v = vectors();
    let a = "00000000-0000-4000-8000-000000000001";
    let b = "ffffffff-0000-4000-8000-000000000002";
    assert_eq!(conversation_context(a, b), s(&v, "signed_payload.dm_context"));
    assert_eq!(conversation_context(b, a), s(&v, "signed_payload.dm_context"));
}

#[test]
fn a_signature_dart_produced_byte_for_byte_and_it_verifies() {
    // Ed25519 is deterministic, so this proves the ladder and the payload at once.
    let v = vectors();
    let identity = server_identity(&v);
    let payload = s(&v, "signed_payload.channel");
    assert_eq!(identity.sign(payload), s(&v, "signature.base64"));
    assert!(verify_signature(payload, s(&v, "signature.base64"), &identity.public_key));
    assert!(!verify_signature(&format!("{payload}x"), s(&v, "signature.base64"), &identity.public_key));
}

#[test]
fn a_bot_opens_what_a_member_sealed_for_it() {
    let v = vectors();
    let identity = derive_chat_identity(&seed(&v), s(&v, "chat_identity.host"));
    assert_eq!(b64(&identity.public_key), s(&v, "chat_identity.public_key_base64"));
    let opened = unwrap_key(&identity, &wrapped(&v)).expect("opens");
    assert_eq!(b64(&opened), s(&v, "wrapped_key.key_base64"));
}

#[test]
fn tampering_with_the_sealed_bytes_is_caught() {
    let v = vectors();
    let identity = derive_chat_identity(&seed(&v), s(&v, "chat_identity.host"));
    let mut bad = wrapped(&v);
    let mut bytes = STANDARD.decode(&bad.ciphertext).unwrap();
    bytes[0] ^= 0xff;
    bad.ciphertext = b64(&bytes);
    assert!(unwrap_key(&identity, &bad).is_none());
}

#[test]
fn the_key_a_bot_speaks_with_is_one_way_from_the_channel_key() {
    let v = vectors();
    let channel_key = STANDARD.decode(s(&v, "wrapped_key.key_base64")).unwrap();
    let derived = hmac(&channel_key, &format!("voicebot:v1:{}", s(&v, "bot_voice_key.bot_id")));
    assert_eq!(b64(&derived), s(&v, "bot_voice_key.key_base64"));
}

#[test]
fn a_bot_derives_the_same_dm_key_the_app_does() {
    let v = vectors();
    let identity = derive_chat_identity(&seed(&v), s(&v, "chat_identity.host"));
    let key = derive_dm_key(&identity, s(&v, "dm_key.peer_chat_public_key")).unwrap();
    assert_eq!(b64(&key), s(&v, "dm_key.key_base64"));

    // A different peer is a different conversation.
    let other = derive_chat_identity(&[9; 32], s(&v, "chat_identity.host"));
    assert_ne!(Some(key), derive_dm_key(&identity, &b64(&other.public_key)));
}

#[test]
fn a_body_round_trips_and_a_bare_string_is_its_own_text() {
    assert_eq!(decode_body(&encode_body("hello")), "hello");
    assert_eq!(decode_body("just text"), "just text");
    assert_eq!(decode_body(r#"{"not":"ours"}"#), r#"{"not":"ours"}"#);
}

#[test]
fn the_three_invite_shapes() {
    let plain = parse_invite("http://localhost:8000#abc123").unwrap();
    assert_eq!((plain.server_url.as_str(), plain.invite_code.as_str()), ("http://localhost:8000", "abc123"));
    // Split on the last `#` alone and the wrapper stays glued to the URL.
    let app = parse_invite("rift://join#http://localhost:8000#abc123").unwrap();
    assert_eq!(app.server_url, "http://localhost:8000");
    let web = parse_invite("https://joinrift.app/join#https://x.supabase.co#dead99").unwrap();
    assert_eq!((web.server_url.as_str(), web.invite_code.as_str()), ("https://x.supabase.co", "dead99"));
    assert_eq!(parse_invite("http://localhost:8000/#abc").unwrap().server_url, "http://localhost:8000");
    for input in ["", "http://localhost:8000", "#abc", "   "] {
        assert!(parse_invite(input).is_none(), "{input:?} parsed as an invite");
    }
}

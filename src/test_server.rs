//! A stand-in for a Rift server's HTTP API, for tests: answers each request
//! from a closure and records what was asked. Plain HTTP/1.1, one request per
//! connection, which is all reqwest needs.

use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::session::BotSession;

/// One request: method, path with its query, and the JSON body if any.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub method: String,
    pub path: String,
    pub body: Value,
}

pub(crate) type Handler = Arc<dyn Fn(&Request) -> (u16, Value) + Send + Sync>;

pub(crate) struct TestServer {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Request>>>,
}

impl TestServer {
    pub async fn start(handler: impl Fn(&Request) -> (u16, Value) + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let handler: Handler = Arc::new(handler);
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut tcp, _)) = listener.accept().await else {
                    return;
                };
                let handler = handler.clone();
                let seen = seen.clone();
                tokio::spawn(async move {
                    let Some(request) = read_request(&mut tcp).await else {
                        return;
                    };
                    seen.lock().unwrap().push(request.clone());
                    let (status, body) = handler(&request);
                    let body = body.to_string();
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = tcp.write_all(response.as_bytes()).await;
                });
            }
        });
        Self { url, requests }
    }

    /// A session signed in as `user_id`, pointed here.
    pub fn session(&self, user_id: &str) -> BotSession {
        BotSession::for_test(&self.url, user_id)
    }

    pub fn paths(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.path.clone())
            .collect()
    }
}

async fn read_request(tcp: &mut tokio::net::TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = tcp.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let path = first.next()?.to_string();
    let length: usize = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + length {
        let n = tcp.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
    Some(Request { method, path, body })
}

// ── Sealing the way a member's client does ──────────────────────────

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};
use serde_json::json;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::crypto::{
    ServerIdentity, b64, derive_server_identity, hmac, random_bytes, signed_payload,
};

/// A member with a signing key of their own.
pub(crate) fn member() -> ServerIdentity {
    derive_server_identity(&random_bytes::<32>(), "localhost", Some("s"))
}

/// A body sealed under `key` and signed by `signer`, as a row's fields.
pub(crate) fn sealed(
    context: &str,
    text: &str,
    key: &[u8; 32],
    version: i64,
    signer: &ServerIdentity,
) -> Value {
    let (ciphertext, nonce) = crate::sealed::seal(text, key);
    json!({
        "ciphertext": ciphertext,
        "nonce": nonce,
        "key_version": version,
        "signature": signer.sign(&signed_payload(context, version, &nonce, &ciphertext)),
    })
}

/// Wrap a key for a chat identity, exactly as a member's client does: the
/// *base64* of the key goes in, which is the trap.
pub(crate) fn wrap_for(their_public: &[u8; 32], key: &[u8]) -> Value {
    let ephemeral = StaticSecret::from(random_bytes::<32>());
    let shared = ephemeral.diffie_hellman(&PublicKey::from(*their_public));
    let wrapping_key = hmac(shared.as_bytes(), "wrap:v1");
    let nonce = random_bytes::<12>();
    let ciphertext = Aes256Gcm::new_from_slice(&wrapping_key)
        .unwrap()
        .encrypt(&Nonce::from(nonce), b64(key).as_bytes())
        .unwrap();
    json!({
        "ephemeral_public_key": b64(PublicKey::from(&ephemeral).as_bytes()),
        "ciphertext": b64(&ciphertext),
        "nonce": b64(&nonce),
    })
}

/// `a` with `b`'s fields added.
pub(crate) fn merged(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
    a
}

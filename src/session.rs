//! A bot's connection to one self-hosted Rift server.
//!
//! There is no bot API to speak of, which is the point: a bot authenticates
//! the way a person does, over the same `login` function, and is subject to
//! the same row-level security. A capability the app has, a bot has, and
//! neither can drift away from the other.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::{Method, RequestBuilder, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::crypto::{ServerIdentity, b64, derive_server_identity, sign_siws};
use crate::error::{BotError, Result};
use crate::invite::parse_invite;
use crate::sealed::{ChatIdentity, derive_chat_identity};

/// How long one request may take, start to end, before it is given up on.
///
/// Without a deadline one request that never answered (the network dropping
/// mid-request was enough) left a bot deaf for good: the poll waits for the
/// last tick before starting the next, so it waited forever.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What a bot needs to come back as itself, minus the seed. Save it beside the
/// seed after a first [`BotSession::join`]: an invite is spent once.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionConfig {
    pub url: String,
    #[serde(rename = "anonKey")]
    pub anon_key: String,
    #[serde(rename = "serverId")]
    pub server_id: String,
}

/// How [`BotSession::join`] introduces the bot.
pub struct JoinOptions<'a> {
    /// The invite link, in any of its three shapes.
    pub invite: &'a str,
    /// 32 random bytes, kept out of the repo. Whoever holds it *is* the bot.
    pub seed: &'a [u8; 32],
    pub username: &'a str,
    pub display_name: Option<&'a str>,
}

#[derive(Default)]
struct Auth {
    anon_key: String,
    token: Option<String>,
    user_id: Option<String>,
}

struct Inner {
    url: String,
    server_id: String,
    identity: ServerIdentity,
    chat_identity: ChatIdentity,
    http: reqwest::Client,
    auth: RwLock<Auth>,
}

/// A signed-in bot on one server. Cheap to clone: clones share one session.
///
/// The seed is the whole identity. It is used once, to derive the keys, and
/// not kept: nothing but signatures ever reaches the server.
#[derive(Clone)]
pub struct BotSession {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for BotSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotSession")
            .field("url", &self.inner.url)
            .field("server_id", &self.inner.server_id)
            .field("user_id", &self.user_id())
            .finish_non_exhaustive()
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("an HTTP client with default TLS")
}

fn host_of(url: &str) -> Result<String> {
    let parsed = url::Url::parse(url).map_err(|e| BotError::new(format!("not a URL: {url}: {e}")))?;
    parsed
        .host_str()
        .map(str::to_string)
        .ok_or_else(|| BotError::new(format!("no host in {url}")))
}

impl BotSession {
    /// A session from a saved [`SessionConfig`] and the seed. Every run after
    /// the first.
    pub fn new(url: &str, anon_key: &str, server_id: &str, seed: &[u8; 32]) -> Result<Self> {
        // The URL's host only: no scheme, no port (WIRE.md §2).
        let host = host_of(url)?;
        Ok(Self {
            inner: Arc::new(Inner {
                url: url.trim_end_matches('/').to_string(),
                server_id: server_id.to_string(),
                identity: derive_server_identity(seed, &host, Some(server_id)),
                // Per host, not per server, and pinned at v1.
                chat_identity: derive_chat_identity(seed, &host),
                http: http_client(),
                auth: RwLock::new(Auth { anon_key: anon_key.to_string(), ..Auth::default() }),
            }),
        })
    }

    pub fn from_config(config: &SessionConfig, seed: &[u8; 32]) -> Result<Self> {
        Self::new(&config.url, &config.anon_key, &config.server_id, seed)
    }

    /// Claim an invite and come back a member. The first run, and only the
    /// first; save [`config`](Self::config) afterwards.
    ///
    /// The order is forced: the identity is scoped to `(host, serverId)`, so
    /// the invite is resolved *before* the keypair exists, and the SIWS login
    /// creates the auth identity that `register` then binds a profile to.
    pub async fn join(options: JoinOptions<'_>) -> Result<Self> {
        let invite = parse_invite(options.invite).ok_or_else(|| {
            BotError::new(
                "Not an invite. Expected \"<server-url>#<code>\", or the rift:// or \
                 https:// form of the same thing.",
            )
        })?;

        let resolved = call_function_with(
            &http_client(),
            &invite.server_url,
            None,
            "resolve_invite",
            &json!({ "invite_code": invite.invite_code }),
        )
        .await?;
        let server_id = resolved["server_id"]
            .as_str()
            .ok_or_else(|| BotError::new("resolve_invite returned no server id"))?;

        // No anon key yet: `login` and `register` are edge functions and need
        // none. Nothing touches a table until `register` hands one back.
        let session = Self::new(&invite.server_url, "", server_id, options.seed)?;
        session.login().await?;

        let context = session
            .call_function(
                "register",
                &json!({
                    "invite_code": invite.invite_code,
                    "public_key": b64(&session.inner.identity.public_key),
                    "stable_id": session.inner.identity.stable_id,
                    "username": options.username,
                    "display_name": options.display_name.unwrap_or(options.username),
                }),
            )
            .await?;
        let anon_key = context["supabase_key"]
            .as_str()
            .ok_or_else(|| BotError::new("register returned no anon key, so no table is readable"))?;
        session.inner.auth.write().unwrap().anon_key = anon_key.to_string();
        // The profile row exists now, and the claims a policy reads come from
        // a fresh token.
        session.login().await?;
        Ok(session)
    }

    pub fn url(&self) -> &str {
        &self.inner.url
    }

    pub fn server_id(&self) -> &str {
        &self.inner.server_id
    }

    pub fn identity(&self) -> &ServerIdentity {
        &self.inner.identity
    }

    /// The X25519 identity members seal this bot's media key and DMs to.
    pub fn chat_identity(&self) -> &ChatIdentity {
        &self.inner.chat_identity
    }

    pub fn anon_key(&self) -> String {
        self.inner.auth.read().unwrap().anon_key.clone()
    }

    /// The JWT requests are signed with, or `None` before the first login.
    pub fn token(&self) -> Option<String> {
        self.inner.auth.read().unwrap().token.clone()
    }

    /// This bot's user id on this server. `None` until the first login.
    pub fn user_id(&self) -> Option<String> {
        self.inner.auth.read().unwrap().user_id.clone()
    }

    pub fn config(&self) -> SessionConfig {
        SessionConfig {
            url: self.inner.url.clone(),
            anon_key: self.anon_key(),
            server_id: self.inner.server_id.clone(),
        }
    }

    pub(crate) fn require_user_id(&self) -> Result<String> {
        self.user_id().ok_or_else(|| BotError::new("not signed in yet: call login first"))
    }

    /// Sign in, or refresh a session that has expired.
    ///
    /// Cheap enough to call before anything: the key comes from the seed, so
    /// there is no stored refresh token to lose. A bot that crashes comes back
    /// as itself.
    pub async fn login(&self) -> Result<()> {
        let (message, signature) = sign_siws(&self.inner.identity);
        let data = self
            .call_function("login", &json!({ "message": message, "signature": signature }))
            .await?;
        let token = data["access_token"]
            .as_str()
            .ok_or_else(|| BotError::new("login returned no token"))?
            .to_string();
        let user_id = subject_of(&token)?;
        let mut auth = self.inner.auth.write().unwrap();
        auth.token = Some(token);
        auth.user_id = Some(user_id);
        Ok(())
    }

    /// Publish this bot's chat public key so members can seal things to it.
    ///
    /// Idempotent. Without it a bot is invisible to the sealing loop:
    /// `get_channel_key` lists only bots that have published one, so it would
    /// wait forever for a media key nobody can produce.
    pub async fn publish_chat_key(&self) -> Result<()> {
        let me = self.require_user_id()?;
        self.patch(
            &format!("users?id=eq.{me}"),
            &json!({ "chat_public_key": b64(&self.inner.chat_identity.public_key) }),
        )
        .await
    }

    /// Publish what this bot answers to, and what it does with what it is given.
    pub async fn publish_manifest(&self, manifest: &Value) -> Result<()> {
        let me = self.require_user_id()?;
        self.patch(&format!("users?id=eq.{me}"), &json!({ "manifest": manifest })).await
    }

    fn rest(&self, method: Method, query: &str) -> RequestBuilder {
        let auth = self.inner.auth.read().unwrap();
        self.inner
            .http
            .request(method, format!("{}/rest/v1/{query}", self.inner.url))
            .header("apikey", &auth.anon_key)
            .bearer_auth(auth.token.as_deref().unwrap_or(""))
    }

    /// Read rows through PostgREST, under this bot's row-level security.
    pub async fn select<T: DeserializeOwned>(&self, query: &str) -> Result<Vec<T>> {
        let res = send(self.rest(Method::GET, query), query).await?;
        json_of(res, query).await
    }

    pub async fn insert(&self, table: &str, row: &Value) -> Result<()> {
        let req = self.rest(Method::POST, table).header("Prefer", "return=minimal").json(row);
        send(req, table).await.map(drop)
    }

    /// An insert that hands the row back: needed only where the id matters.
    pub async fn insert_returning<T: DeserializeOwned>(&self, table: &str, row: &Value) -> Result<Vec<T>> {
        let req = self.rest(Method::POST, table).header("Prefer", "return=representation").json(row);
        json_of(send(req, table).await?, table).await
    }

    /// Call a database function. The permission check happens inside the
    /// database, against this bot's JWT; an edge function is
    /// [`call_function`](Self::call_function).
    pub async fn rpc<T: DeserializeOwned>(&self, name: &str, params: &Value) -> Result<Option<T>> {
        let path = format!("rpc/{name}");
        let res = send(self.rest(Method::POST, &path).json(params), &path).await?;
        let text = res.text().await.map_err(|e| BotError::new(format!("{path}: {e}")))?;
        if text.is_empty() {
            return Ok(None);
        }
        serde_json::from_str(&text).map(Some).map_err(|e| BotError::new(format!("{path}: {e}")))
    }

    pub async fn patch(&self, query: &str, body: &Value) -> Result<()> {
        let req = self.rest(Method::PATCH, query).header("Prefer", "return=minimal").json(body);
        send(req, query).await.map(drop)
    }

    pub async fn remove(&self, query: &str) -> Result<()> {
        let req = self.rest(Method::DELETE, query).header("Prefer", "return=minimal");
        send(req, query).await.map(drop)
    }

    /// Send one Realtime broadcast without joining its topic: somebody else's
    /// `user:` topic, which the rules let a member send to but never hear.
    pub async fn broadcast(&self, topic: &str, event: &str, payload: &Value) -> Result<()> {
        let req = {
            let auth = self.inner.auth.read().unwrap();
            self.inner
                .http
                .post(format!("{}/realtime/v1/api/broadcast", self.inner.url))
                .header("apikey", &auth.anon_key)
                .bearer_auth(auth.token.as_deref().unwrap_or(""))
                .json(&json!({
                    "messages": [{ "topic": topic, "event": event, "payload": payload, "private": true }]
                }))
        };
        send(req, "broadcast").await.map(drop)
    }

    fn storage(&self, method: Method, bucket: &str, path: &str) -> RequestBuilder {
        let auth = self.inner.auth.read().unwrap();
        self.inner
            .http
            .request(method, format!("{}/storage/v1/object/{bucket}/{path}", self.inner.url))
            .header("apikey", &auth.anon_key)
            .bearer_auth(auth.token.as_deref().unwrap_or(""))
    }

    /// Put a file in one of this server's Storage buckets. Refuses to
    /// overwrite: a name is used once, so a client holding the old picture is
    /// never shown a different one under it.
    pub async fn upload_object(&self, bucket: &str, path: &str, bytes: Vec<u8>, content_type: &str) -> Result<()> {
        let req = self
            .storage(Method::POST, bucket, path)
            .header("Content-Type", content_type)
            .header("x-upsert", "false")
            .body(bytes);
        send(req, "storage").await.map(drop)
    }

    /// Delete a file this bot uploaded. Storage lets an uploader remove its own.
    pub async fn remove_object(&self, bucket: &str, path: &str) -> Result<()> {
        send(self.storage(Method::DELETE, bucket, path), "storage").await.map(drop)
    }

    /// Call an edge function. They answer 200 with `{success, data, error}`.
    pub async fn call_function(&self, name: &str, body: &Value) -> Result<Value> {
        let token = self.token();
        call_function_with(&self.inner.http, &self.inner.url, token.as_deref(), name, body).await
    }
}

async fn call_function_with(
    http: &reqwest::Client,
    url: &str,
    token: Option<&str>,
    name: &str,
    body: &Value,
) -> Result<Value> {
    let mut req = http.post(format!("{url}/functions/v1/{name}")).json(body);
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }
    let res = req.send().await.map_err(|e| BotError::new(format!("{name}: {}", plain(e))))?;
    let status = res.status().as_u16();
    let json: Value = res.json().await.map_err(|e| BotError::new(format!("{name}: {}", plain(e))))?;
    if json["success"] != Value::Bool(true) {
        let reason = json["error"].as_str().map(str::to_string).unwrap_or_else(|| status.to_string());
        return Err(BotError::with_status(format!("{name} failed: {reason}"), status));
    }
    Ok(json["data"].clone())
}

/// A reqwest error without the URL in it: a query string can carry ids, and
/// the path is said by whoever reports it.
fn plain(error: reqwest::Error) -> String {
    if error.is_timeout() { "timed out".to_string() } else { error.without_url().to_string() }
}

async fn send(req: RequestBuilder, what: &str) -> Result<Response> {
    let res = req.send().await.map_err(|e| BotError::new(format!("{}: {}", path_of(what), plain(e))))?;
    if res.status().is_success() {
        return Ok(res);
    }
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    Err(BotError::with_status(format!("{status}: {text}"), status))
}

async fn json_of<T: DeserializeOwned>(res: Response, what: &str) -> Result<T> {
    res.json().await.map_err(|e| BotError::new(format!("{}: {}", path_of(what), plain(e))))
}

fn path_of(query: &str) -> &str {
    query.split('?').next().unwrap_or(query)
}

/// The `sub` claim: this bot's user id, which `users.id` is.
fn subject_of(token: &str) -> Result<String> {
    let payload = token
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .ok_or_else(|| BotError::new("login returned a token that is not a JWT"))?;
    payload["sub"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| BotError::new("login returned a token with no subject"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_subject_comes_out_of_the_token() {
        let claims = URL_SAFE_NO_PAD.encode(br#"{"sub":"abc-123","role":"authenticated"}"#);
        assert_eq!(subject_of(&format!("x.{claims}.y")).unwrap(), "abc-123");
        assert!(subject_of("nonsense").is_err());
    }

    #[test]
    fn the_host_is_the_host_only() {
        assert_eq!(host_of("https://chat.example.org:8443/rift").unwrap(), "chat.example.org");
    }

    #[test]
    fn the_config_reads_what_the_typescript_sdk_saved() {
        let saved = r#"{"url":"http://localhost:8000","anonKey":"k","serverId":"s"}"#;
        let config: SessionConfig = serde_json::from_str(saved).unwrap();
        assert_eq!(config.anon_key, "k");
    }
}

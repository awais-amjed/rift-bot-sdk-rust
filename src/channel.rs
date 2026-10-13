//! Reading a channel the bot has been granted: the one exception in BOTS.md §6.
//!
//! **This is the only thing a bot does that a bot is not supposed to do.**
//! Everywhere else it hears what it is told. Here it holds the channel key and
//! reads a conversation nobody addressed to it, because a moderation bot cannot
//! work any other way.
//!
//! What makes that safe to offer is all in the database, so a bug in this file
//! cannot widen it: an admin granted it per channel; the grant is forward-only
//! (`from_key_version`); revoking rotates the key and stops the reading in the
//! same statement; and the channel says so where everybody can see it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::body::decode_body;
use crate::crypto::{signed_payload, verify_signature};
use crate::error::Result;
use crate::sealed::{Wrapped, open, unwrap_key};
use crate::senders::SenderKeys;
use crate::session::BotSession;

/// How long to wait before asking the keyring again for a version this bot
/// does not hold: an unsealed rotation is not a round trip per message, and a
/// bot still picks up the seal within a human moment of a member opening the
/// channel.
const KEY_REFETCH: Duration = Duration::from_secs(30);

/// One channel message, opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMessage {
    pub id: i64,
    pub channel_id: String,
    pub sender_id: String,
    pub text: String,
    pub sent_at: String,
    /// Which key version it was sealed under.
    pub key_version: i64,
}

#[derive(Deserialize)]
struct MessageRow {
    id: i64,
    channel_id: String,
    sender_id: Option<String>,
    ciphertext: String,
    nonce: String,
    signature: Option<String>,
    key_version: i64,
    created_at: String,
}

#[derive(Deserialize)]
struct KeyRow {
    key_version: i64,
    #[serde(flatten)]
    wrapped: Wrapped,
}

#[derive(Deserialize)]
struct GrantRow {
    from_key_version: i64,
}

#[derive(Deserialize)]
struct IdRow {
    id: i64,
}

struct Keys {
    /// Channel keys by version, unwrapped once. A rotation adds one; none is
    /// replaced, because scrollback under the old version stays readable.
    by_version: HashMap<i64, Vec<u8>>,
    loaded_at: Option<Instant>,
}

pub struct ChannelReader {
    session: BotSession,
    channel_id: String,
    keys: Mutex<Keys>,
    senders: SenderKeys,
}

impl ChannelReader {
    pub fn new(session: BotSession, channel_id: &str) -> Self {
        Self {
            senders: SenderKeys::new(session.clone()),
            session,
            channel_id: channel_id.to_string(),
            keys: Mutex::new(Keys { by_version: HashMap::new(), loaded_at: None }),
        }
    }

    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// The first key version this bot may hold here, or `None` if it was never
    /// granted this channel. An ungranted bot polling forever receives nothing
    /// and cannot tell that from a quiet channel; this is the difference.
    pub async fn granted_from(&self) -> Result<Option<i64>> {
        let me = self.session.require_user_id()?;
        let rows: Vec<GrantRow> = self
            .session
            .select(&format!(
                "bot_channel_keys?select=from_key_version&channel_id=eq.{}&bot_id=eq.{me}",
                self.channel_id
            ))
            .await?;
        Ok(rows.first().map(|r| r.from_key_version))
    }

    /// The newest id here, so a restart does not replay a backlog.
    ///
    /// Through `channel_messages` rather than the table: every channel's
    /// messages share one sequence, so `order=id.desc&limit=1` on the table
    /// walks everything said anywhere since this channel last spoke. The RPC
    /// takes it off the channel's own index.
    pub async fn newest_id(&self) -> Result<i64> {
        let rows: Vec<IdRow> = self
            .session
            .select(&format!("rpc/channel_messages?p_channel={}&p_limit=1&select=id", self.channel_id))
            .await?;
        Ok(rows.first().map_or(0, |r| r.id))
    }

    /// Everything readable here after `after_id`, oldest first.
    ///
    /// **Stops at the first message whose key has not arrived yet**, rather
    /// than skipping it. A rotation is sealed for this bot by the next member
    /// to open the channel, so a version can exist for a moment before its key
    /// does, and a caller that moved past those rows would drop that stretch
    /// of conversation for good. A row unopenable for any other reason
    /// (unsigned, wrongly signed, never sealed) is skipped: waiting cannot
    /// change it.
    pub async fn since(&self, after_id: i64) -> Result<Vec<ChannelMessage>> {
        let rows: Vec<MessageRow> = self
            .session
            .select(&format!(
                "messages?select=id,channel_id,sender_id,ciphertext,nonce,signature,key_version,created_at\
                 &channel_id=eq.{}&id=gt.{after_id}&order=id.asc",
                self.channel_id
            ))
            .await?;
        let mut out = Vec::new();
        for row in rows {
            // A row with no sender is a webhook post or a system notice:
            // neither sealed nor signed, and withheld by the policy anyway.
            let Some(sender_id) = row.sender_id.clone() else { continue };
            let Some(signature) = row.signature.clone() else { continue };
            if row.key_version < 1 {
                continue;
            }
            let Some(key) = self.key_for(row.key_version).await? else { break };
            if let Some(message) = self.open_row(row, &sender_id, &signature, &key).await? {
                out.push(message);
            }
        }
        Ok(out)
    }

    async fn open_row(
        &self,
        row: MessageRow,
        sender_id: &str,
        signature: &str,
        key: &[u8],
    ) -> Result<Option<ChannelMessage>> {
        let payload = signed_payload(&self.channel_id, row.key_version, &row.nonce, &row.ciphertext);
        let Some(sender_key) = self.senders.public_key_for(sender_id).await? else { return Ok(None) };
        if !verify_signature(&payload, signature, &sender_key) {
            return Ok(None);
        }
        Ok(open(&row.ciphertext, &row.nonce, key).map(|plain| ChannelMessage {
            id: row.id,
            channel_id: row.channel_id,
            sender_id: sender_id.to_string(),
            text: decode_body(&plain),
            sent_at: row.created_at,
            key_version: row.key_version,
        }))
    }

    /// The channel key for one version. A miss refetches the keyring, but no
    /// more than once every [`KEY_REFETCH`]: the seal arrives whenever the next
    /// member opens the channel, which may be hours.
    async fn key_for(&self, version: i64) -> Result<Option<Vec<u8>>> {
        {
            let keys = self.keys.lock().unwrap();
            if let Some(key) = keys.by_version.get(&version) {
                return Ok(Some(key.clone()));
            }
            if keys.loaded_at.is_some_and(|at| at.elapsed() < KEY_REFETCH) {
                return Ok(None);
            }
        }
        self.load_keys().await?;
        Ok(self.keys.lock().unwrap().by_version.get(&version).cloned())
    }

    async fn load_keys(&self) -> Result<()> {
        let me = self.session.require_user_id()?;
        self.keys.lock().unwrap().loaded_at = Some(Instant::now());
        let rows: Vec<KeyRow> = self
            .session
            .select(&format!(
                "channel_keyring?select=key_version,ephemeral_public_key,ciphertext,nonce\
                 &channel_id=eq.{}&user_id=eq.{me}",
                self.channel_id
            ))
            .await?;
        let mut keys = self.keys.lock().unwrap();
        for row in rows {
            if keys.by_version.contains_key(&row.key_version) {
                continue;
            }
            // Sealed to a chat key this bot no longer has: that version stays
            // unreadable rather than taking the whole channel down.
            if let Some(key) = unwrap_key(self.session.chat_identity(), &row.wrapped) {
                keys.by_version.insert(row.key_version, key);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use super::*;
    use crate::crypto::{b64, random_bytes};
    use crate::test_server::{TestServer, member, merged, sealed, wrap_for};

    const CHANNEL: &str = "cccccccc-0000-4000-8000-000000000001";
    const BOT: &str = "bbbbbbbb-0000-4000-8000-000000000001";
    const MEMBER: &str = "aaaaaaaa-0000-4000-8000-000000000001";

    struct Fixture {
        server: TestServer,
        rows: Arc<Mutex<Vec<Value>>>,
        keyring: Arc<Mutex<Vec<Value>>>,
        reader: ChannelReader,
    }

    async fn fixture(author_public: [u8; 32]) -> Fixture {
        let rows = Arc::new(Mutex::new(Vec::new()));
        let keyring = Arc::new(Mutex::new(Vec::new()));
        let (r, k) = (rows.clone(), keyring.clone());
        let server = TestServer::start(move |req| {
            let path = req.path.trim_start_matches("/rest/v1/");
            let body = if path.starts_with("messages?") {
                Value::Array(r.lock().unwrap().clone())
            } else if path.starts_with("channel_keyring?") {
                Value::Array(k.lock().unwrap().clone())
            } else if path.starts_with("users?") {
                json!([{ "public_key": b64(&author_public) }])
            } else {
                json!([])
            };
            (200, body)
        })
        .await;
        let reader = ChannelReader::new(server.session(BOT), CHANNEL);
        Fixture { server, rows, keyring, reader }
    }

    impl Fixture {
        fn grant(&self, version: i64, key: &[u8; 32]) {
            let wrapped = wrap_for(&self.reader.session.chat_identity().public_key, key);
            self.keyring.lock().unwrap().push(merged(json!({ "key_version": version }), wrapped));
        }

        fn row(&self, id: i64, fields: Value) {
            let base = json!({ "id": id, "channel_id": CHANNEL, "sender_id": MEMBER, "created_at": "x" });
            self.rows.lock().unwrap().push(merged(base, fields));
        }

        fn ids(messages: &[ChannelMessage]) -> Vec<i64> {
            messages.iter().map(|m| m.id).collect()
        }
    }

    #[tokio::test]
    async fn a_granted_bot_opens_what_was_sealed_under_a_version_it_holds() {
        let author = member();
        let f = fixture(author.public_key).await;
        let key = random_bytes::<32>();
        f.grant(2, &key);
        f.row(7, sealed(CHANNEL, "the quiet part", &key, 2, &author));

        let out = f.reader.since(0).await.unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].text.as_str(), out[0].sender_id.as_str(), out[0].key_version), ("the quiet part", MEMBER, 2));
    }

    /// The whole attack: a bot that acts on a forged row can be told to do
    /// anything by anyone who can write to the table.
    #[tokio::test]
    async fn a_row_signed_by_somebody_else_is_dropped() {
        let author = member();
        let f = fixture(author.public_key).await;
        let key = random_bytes::<32>();
        f.grant(2, &key);
        f.row(8, sealed(CHANNEL, "transfer everything", &key, 2, &member()));
        assert!(f.reader.since(0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn it_stops_at_a_version_whose_key_has_not_been_sealed_yet() {
        let author = member();
        let f = fixture(author.public_key).await;
        let two = random_bytes::<32>();
        f.grant(2, &two);
        f.row(9, sealed(CHANNEL, "before the rotation", &two, 2, &author));
        f.row(10, sealed(CHANNEL, "after it", &random_bytes::<32>(), 3, &author));
        f.row(11, sealed(CHANNEL, "and after that", &two, 2, &author));
        assert_eq!(Fixture::ids(&f.reader.since(0).await.unwrap()), [9]);
    }

    /// Webhook posts and notices are version 0 and never sealed. No waiting
    /// produces a key for them, so they must not stop the cursor.
    #[tokio::test]
    async fn a_plaintext_row_is_skipped_rather_than_stalling_the_reader() {
        let author = member();
        let f = fixture(author.public_key).await;
        let key = random_bytes::<32>();
        f.grant(2, &key);
        f.row(12, json!({ "sender_id": null, "ciphertext": "build passed", "nonce": "",
                          "signature": null, "key_version": 0 }));
        f.row(13, sealed(CHANNEL, "still reading", &key, 2, &author));
        assert_eq!(Fixture::ids(&f.reader.since(0).await.unwrap()), [13]);
    }

    #[tokio::test]
    async fn an_ungranted_channel_says_so_instead_of_going_quiet() {
        let f = fixture(member().public_key).await;
        assert_eq!(f.reader.granted_from().await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_keyring_is_fetched_once_not_once_per_message() {
        let author = member();
        let f = fixture(author.public_key).await;
        let key = random_bytes::<32>();
        f.grant(2, &key);
        for id in 20..25 {
            f.row(id, sealed(CHANNEL, &format!("m{id}"), &key, 2, &author));
        }
        assert_eq!(f.reader.since(0).await.unwrap().len(), 5);
        let fetches = f.server.paths().iter().filter(|p| p.contains("/channel_keyring?")).count();
        assert_eq!(fetches, 1);
    }
}

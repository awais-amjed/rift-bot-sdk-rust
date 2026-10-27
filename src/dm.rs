//! Direct messages to a bot.
//!
//! A DM to a bot is private **from the server**, not from the bot. It is
//! sealed exactly like a DM between two people, and the bot is simply one of
//! the two ends. That makes it the one place a bot does real chat crypto:
//!
//! ```text
//! dmKey = HMAC-SHA256(X25519(myChatPrivate, theirChatPublic), "dm:v1")
//! ```
//!
//! Both ends derive the same bytes from opposite halves, so there is nothing
//! to distribute and nothing for the server to hold.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;
use serde_json::json;

use crate::body::{decode_body, encode_body};
use crate::crypto::{conversation_context, signed_payload, verify_signature};
use crate::error::{BotError, Result};
use crate::sealed::{derive_dm_key, open, seal};
use crate::senders::SenderKeys;
use crate::session::BotSession;

/// One direct message, opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectMessage {
    pub id: i64,
    /// Who sent it: the person on the other end.
    pub peer_id: String,
    pub text: String,
    pub sent_at: String,
}

#[derive(Deserialize)]
struct DmRow {
    id: i64,
    sender_id: String,
    recipient_id: String,
    ciphertext: String,
    nonce: String,
    key_version: i64,
    signature: Option<String>,
    created_at: String,
}

#[derive(Deserialize)]
struct IdRow {
    id: i64,
}

#[derive(Deserialize)]
struct ChatKeyRow {
    chat_public_key: Option<String>,
}

/// Read and answer direct messages.
///
/// Held apart from channel replies because the two have different reach: a
/// bot in a channel is spoken to in front of everybody and answers there, and
/// a DM is a conversation with one person that no member and no server can
/// read. Keeping them apart is what stops a reply going to the wrong one.
pub struct DirectMessages {
    session: BotSession,
    /// Conversation keys, by peer id. Derived once; they never change.
    keys: Mutex<HashMap<String, [u8; 32]>>,
    senders: SenderKeys,
}

impl DirectMessages {
    pub fn new(session: BotSession) -> Self {
        Self {
            senders: SenderKeys::new(session.clone()),
            session,
            keys: Mutex::new(HashMap::new()),
        }
    }

    /// Everything sent to this bot after `after_id`, oldest first. A row that
    /// cannot be trusted or opened is left out.
    pub async fn since(&self, after_id: i64) -> Result<Vec<DirectMessage>> {
        let me = self.session.require_user_id()?;
        let rows: Vec<DmRow> = self
            .session
            .select(&format!(
                "dm_messages?select=id,sender_id,recipient_id,ciphertext,nonce,key_version,signature,created_at\
                 &recipient_id=eq.{me}&id=gt.{after_id}&order=id.asc"
            ))
            .await?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(message) = self.open_row(row).await? {
                out.push(message);
            }
        }
        Ok(out)
    }

    /// The newest id addressed to this bot, so a restart starts from *now*.
    pub async fn newest_id(&self) -> Result<i64> {
        let me = self.session.require_user_id()?;
        let rows: Vec<IdRow> = self
            .session
            .select(&format!(
                "dm_messages?select=id&recipient_id=eq.{me}&order=id.desc&limit=1"
            ))
            .await?;
        Ok(rows.first().map_or(0, |r| r.id))
    }

    /// Answer one, sealed to the same conversation.
    pub async fn reply(&self, to: &DirectMessage, text: &str) -> Result<()> {
        self.send(&to.peer_id, text).await
    }

    /// Write to somebody, sealed to the two of you. They must have opened Rift
    /// once, or there is no chat key to seal to.
    pub async fn send(&self, peer_id: &str, text: &str) -> Result<()> {
        let me = self.session.require_user_id()?;
        let key = self.key_for(peer_id).await?;
        let (ciphertext, nonce) = seal(&encode_body(text), &key);
        let payload = signed_payload(&conversation_context(&me, peer_id), 1, &nonce, &ciphertext);
        self.session
            .insert(
                "dm_messages",
                &json!({
                    "recipient_id": peer_id,
                    "ciphertext": ciphertext,
                    "nonce": nonce,
                    "key_version": 1,
                    "signature": self.session.identity().sign(&payload),
                }),
            )
            .await
    }

    /// Open one row, or `None` if it cannot be trusted.
    ///
    /// Both ways a row is dropped are silent on purpose. A signature that does
    /// not check out means the row is not from who it claims, and a bot acting
    /// on it is the whole attack. A body that will not decrypt is one sealed
    /// to a key this bot does not have: a message it was never part of.
    async fn open_row(&self, row: DmRow) -> Result<Option<DirectMessage>> {
        let Some(signature) = row.signature.as_deref() else {
            return Ok(None);
        };
        let Some(sender_key) = self.senders.public_key_for(&row.sender_id).await? else {
            return Ok(None);
        };
        let context = conversation_context(&row.sender_id, &row.recipient_id);
        let payload = signed_payload(&context, row.key_version, &row.nonce, &row.ciphertext);
        if !verify_signature(&payload, signature, &sender_key) {
            return Ok(None);
        }
        let key = match self.key_for(&row.sender_id).await {
            Ok(key) => key,
            Err(_) => return Ok(None),
        };
        Ok(
            open(&row.ciphertext, &row.nonce, &key).map(|plain| DirectMessage {
                id: row.id,
                peer_id: row.sender_id,
                text: decode_body(&plain),
                sent_at: row.created_at,
            }),
        )
    }

    async fn key_for(&self, peer_id: &str) -> Result<[u8; 32]> {
        if let Some(key) = self.keys.lock().unwrap().get(peer_id) {
            return Ok(*key);
        }
        let rows: Vec<ChatKeyRow> = self
            .session
            .select(&format!("users?select=chat_public_key&id=eq.{peer_id}"))
            .await?;
        let theirs = rows
            .into_iter()
            .next()
            .and_then(|r| r.chat_public_key)
            .ok_or_else(|| {
                BotError::new(format!(
                    "{peer_id} has published no chat key, so there is no conversation to open"
                ))
            })?;
        let key = derive_dm_key(self.session.chat_identity(), &theirs)
            .ok_or_else(|| BotError::new(format!("{peer_id}'s chat key is not an X25519 key")))?;
        self.keys.lock().unwrap().insert(peer_id.to_string(), key);
        Ok(key)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::{Value, json};

    use super::*;
    use crate::crypto::{b64, random_bytes};
    use crate::sealed::derive_chat_identity;
    use crate::test_server::{TestServer, member, merged, sealed};

    const BOT: &str = "bbbbbbbb-0000-4000-8000-000000000001";
    const PEER: &str = "aaaaaaaa-0000-4000-8000-000000000001";

    /// A peer DMs the bot, and the bot answers: both ends derive one key, the
    /// bot opens the row, and its answer opens with the peer's half.
    #[tokio::test]
    async fn a_dm_opens_and_the_answer_is_sealed_to_the_same_conversation() {
        let peer_sign = member();
        let peer_chat = derive_chat_identity(&random_bytes::<32>(), "localhost");
        let rows = Arc::new(Mutex::new(Vec::<Value>::new()));
        let inserted = Arc::new(Mutex::new(Vec::<Value>::new()));
        let (r, i) = (rows.clone(), inserted.clone());
        let (sign_pub, chat_pub) = (peer_sign.public_key, peer_chat.public_key);
        let server = TestServer::start(move |req| {
            let path = req.path.trim_start_matches("/rest/v1/");
            if req.method == "POST" {
                i.lock().unwrap().push(req.body.clone());
                return (201, json!(null));
            }
            let body = if path.starts_with("dm_messages?") {
                Value::Array(r.lock().unwrap().clone())
            } else if path.starts_with("users?select=public_key") {
                json!([{ "public_key": b64(&sign_pub) }])
            } else if path.starts_with("users?select=chat_public_key") {
                json!([{ "chat_public_key": b64(&chat_pub) }])
            } else {
                json!([])
            };
            (200, body)
        })
        .await;
        let session = server.session(BOT);
        let dms = DirectMessages::new(session.clone());

        let key = derive_dm_key(&peer_chat, &b64(&session.chat_identity().public_key)).unwrap();
        let context = conversation_context(PEER, BOT);
        let base = json!({ "id": 3, "sender_id": PEER, "recipient_id": BOT, "created_at": "x" });
        rows.lock().unwrap().push(merged(
            base.clone(),
            sealed(&context, &encode_body("hi bot"), &key, 1, &peer_sign),
        ));
        // A forged one beside it is left out.
        let forged = merged(
            json!({ "id": 4 }),
            sealed(&context, &encode_body("lies"), &key, 1, &member()),
        );
        rows.lock().unwrap().push(merged(base, forged));

        let got = dms.since(0).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            (got[0].text.as_str(), got[0].peer_id.as_str()),
            ("hi bot", PEER)
        );

        dms.reply(&got[0], "hello you").await.unwrap();
        let row = inserted.lock().unwrap()[0].clone();
        let opened = open(
            row["ciphertext"].as_str().unwrap(),
            row["nonce"].as_str().unwrap(),
            &key,
        )
        .unwrap();
        assert_eq!(decode_body(&opened), "hello you");
        let payload = signed_payload(
            &context,
            1,
            row["nonce"].as_str().unwrap(),
            row["ciphertext"].as_str().unwrap(),
        );
        assert!(verify_signature(
            &payload,
            row["signature"].as_str().unwrap(),
            &session.identity().public_key
        ));
    }
}

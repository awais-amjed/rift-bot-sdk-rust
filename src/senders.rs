use std::collections::HashMap;
use std::sync::Mutex;

use serde::Deserialize;

use crate::crypto::unb64;
use crate::error::Result;
use crate::session::BotSession;

/// Ed25519 public keys, by user, fetched once.
///
/// Every message is verified before a bot acts on it, so this is asked for on
/// every row of every poll, and a member's signing key does not change under a
/// running process. A miss is cached too, so an author who has published no
/// key is not re-fetched forever.
pub(crate) struct SenderKeys {
    session: BotSession,
    keys: Mutex<HashMap<String, Option<Vec<u8>>>>,
}

#[derive(Deserialize)]
struct Row {
    public_key: Option<String>,
}

impl SenderKeys {
    pub(crate) fn new(session: BotSession) -> Self {
        Self { session, keys: Mutex::new(HashMap::new()) }
    }

    pub(crate) async fn public_key_for(&self, user_id: &str) -> Result<Option<Vec<u8>>> {
        if let Some(cached) = self.keys.lock().unwrap().get(user_id) {
            return Ok(cached.clone());
        }
        let rows: Vec<Row> = self.session.select(&format!("users?select=public_key&id=eq.{user_id}")).await?;
        let key = rows.into_iter().next().and_then(|r| r.public_key).and_then(|k| unb64(&k));
        self.keys.lock().unwrap().insert(user_id.to_string(), key.clone());
        Ok(key)
    }
}

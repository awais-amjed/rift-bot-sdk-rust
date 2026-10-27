//! Where this bot is in voice, said the way a member's app says it.
//!
//! A member's sidebar draws who is in which voice channel from two things: a
//! roster read from LiveKit when the app connects, and a broadcast on
//! `voice:<server id>` each time somebody moves. A bot used to say nothing, so
//! it was drawn from the roster read alone. A member whose app connected before
//! the bot arrived did not see it come in, and one whose app read the roster
//! while the bot was there saw it stay after it left (online, so still
//! believed) until that member restarted the app. So a bot says where it is on
//! joining and on leaving, like everybody else.
//!
//! The message is the app's own (`VoiceLocations` in `rift-app`): `here`, with
//! `{v: 1, userId, channelId}`, a null channel meaning "in none".

use serde_json::{Value, json};

use crate::session::BotSession;

pub(crate) const VOICE_LOCATION_EVENT: &str = "here";
pub(crate) const VOICE_LOCATION_VERSION: u32 = 1;

pub(crate) fn voice_location_topic(server_id: &str) -> String {
    format!("voice:{server_id}")
}

pub(crate) fn voice_location(user_id: &str, channel_id: Option<&str>) -> Value {
    json!({ "v": VOICE_LOCATION_VERSION, "userId": user_id, "channelId": channel_id })
}

/// The voice channels one session is connected to, oldest first.
///
/// A member's app holds one channel per person, so a bot in two calls at once
/// can only be drawn in one of them. It is the newest: leaving that one moves
/// it to the one before, and only leaving the last says "in none".
#[derive(Debug, Default)]
pub(crate) struct VoicePlaces {
    channels: Vec<String>,
}

impl VoicePlaces {
    /// Joined `channel_id`: where to say this bot is now.
    pub(crate) fn enter(&mut self, channel_id: &str) -> Option<String> {
        self.channels.push(channel_id.to_string());
        Some(channel_id.to_string())
    }

    /// Left `channel_id`: where to say this bot is now, or `None` for nowhere.
    pub(crate) fn leave(&mut self, channel_id: &str) -> Option<String> {
        if let Some(at) = self.channels.iter().rposition(|c| c == channel_id) {
            self.channels.remove(at);
        }
        self.channels.last().cloned()
    }
}

/// Move this bot's place with `change` and tell the server's members.
///
/// One message at a time per session, in order: the lock is held across the
/// send, so a join and a quick leave cannot arrive the wrong way round and
/// leave the bot drawn in a call it had already left.
///
/// Never fails. Losing this message is what every bot did before it existed,
/// and a member's next roster read puts it right; failing a join or a leave
/// over it would be worse.
pub(crate) async fn announce_place(
    session: &BotSession,
    change: impl FnOnce(&mut VoicePlaces) -> Option<String>,
) {
    let mut places = session.voice_places().lock().await;
    let channel_id = change(&mut places);
    let Some(user_id) = session.user_id() else {
        return;
    };
    let topic = voice_location_topic(session.server_id());
    let payload = voice_location(&user_id, channel_id.as_deref());
    let mut result = session
        .broadcast(&topic, VOICE_LOCATION_EVENT, &payload)
        .await;
    // An hour into a song the session has run out. Once more, signed in again.
    if matches!(&result, Err(e) if e.status == Some(401)) {
        result = match session.login().await {
            Ok(()) => {
                session
                    .broadcast(&topic, VOICE_LOCATION_EVENT, &payload)
                    .await
            }
            Err(e) => Err(e),
        };
    }
    if let Err(e) = result {
        log::warn!("voice: could not tell members where the bot is: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;
    use crate::test_server::TestServer;

    #[test]
    fn the_message_is_the_apps() {
        assert_eq!(VOICE_LOCATION_EVENT, "here");
        assert_eq!(voice_location_topic("s"), "voice:s");
        assert_eq!(
            voice_location("u", Some("c")),
            json!({ "v": 1, "userId": "u", "channelId": "c" })
        );
        assert_eq!(
            voice_location("u", None),
            json!({ "v": 1, "userId": "u", "channelId": null })
        );
    }

    #[test]
    fn a_bot_in_two_calls_is_drawn_in_the_newest_and_leaves_to_the_one_before() {
        let mut places = VoicePlaces::default();
        assert_eq!(places.enter("a").as_deref(), Some("a"));
        assert_eq!(places.enter("b").as_deref(), Some("b"));
        assert_eq!(places.leave("b").as_deref(), Some("a"));
        assert_eq!(places.leave("a"), None);
        // Leaving somewhere it never was moves nothing.
        assert_eq!(places.leave("z"), None);
    }

    #[test]
    fn two_connections_to_one_channel_leaving_one_keeps_it_there() {
        let mut places = VoicePlaces::default();
        places.enter("a");
        places.enter("a");
        assert_eq!(places.leave("a").as_deref(), Some("a"));
        assert_eq!(places.leave("a"), None);
    }

    #[tokio::test]
    async fn joining_then_leaving_says_both_in_order_on_the_voice_topic() {
        let server = TestServer::start(|_| (202, Value::Null)).await;
        let session = server.session("bot-1");
        announce_place(&session, |p| p.enter("chan")).await;
        announce_place(&session, |p| p.leave("chan")).await;
        let sent: Vec<Value> = server
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.path == "/realtime/v1/api/broadcast")
            .map(|r| r.body["messages"][0].clone())
            .collect();
        assert_eq!(
            sent,
            vec![
                json!({ "topic": "voice:server-1", "event": "here", "private": true,
                        "payload": { "v": 1, "userId": "bot-1", "channelId": "chan" } }),
                json!({ "topic": "voice:server-1", "event": "here", "private": true,
                        "payload": { "v": 1, "userId": "bot-1", "channelId": null } }),
            ]
        );
    }

    #[tokio::test]
    async fn a_refused_message_does_not_fail_the_caller() {
        let refused = Arc::new(AtomicBool::new(false));
        let seen = refused.clone();
        let server = TestServer::start(move |_| {
            seen.store(true, Ordering::SeqCst);
            (500, json!({}))
        })
        .await;
        announce_place(&server.session("bot-1"), |p| p.enter("chan")).await;
        assert!(refused.load(Ordering::SeqCst));
    }
}

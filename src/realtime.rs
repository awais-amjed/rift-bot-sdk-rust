//! Listening to a Rift server's Realtime.
//!
//! The database announces what a bot may hear on its own topic,
//! `user:<bot id>`, which nobody else may join. Realtime speaks Phoenix's
//! protocol over a WebSocket: join a topic, hold it open with a heartbeat, and
//! read the frames that come back.
//!
//! What arrives is a nudge, never the message: the payload is ids, and the bot
//! reads the row it names through the same queries and the same policies as
//! before. A dropped frame therefore costs latency and nothing else, which is
//! why [`Bot`](crate::Bot) keeps a slow poll behind this.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

use crate::error::BotError;

/// One broadcast: what happened, and the ids it happened to.
#[derive(Debug, Clone, PartialEq)]
pub struct RealtimeEvent {
    pub event: String,
    pub payload: Value,
}

/// What a listener hands on.
#[derive(Debug)]
pub enum Heard {
    Event(RealtimeEvent),
    /// A connection that failed or was refused. Never fatal: it reconnects.
    Error(BotError),
}

const HEARTBEAT: Duration = Duration::from_secs(25);

/// Backoff between reconnects: quick at first, then out of the way.
const RETRY: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

pub struct RealtimeOptions {
    /// The server's URL, as the session holds it (`https://…`).
    pub url: String,
    /// The server's publishable key.
    pub anon_key: String,
    /// The bot's JWT. Replaceable while connected: see
    /// [`RealtimeListener::set_token`].
    pub token: String,
    /// The topic to join, without Phoenix's `realtime:` prefix.
    pub topic: String,
    /// What to announce on this topic's presence for as long as it is joined:
    /// how a member's app knows somebody is online. `None` only listens.
    pub presence: Option<Value>,
}

/// What one frame from the server means to a listener.
#[derive(Debug, PartialEq)]
enum Incoming {
    Broadcast(RealtimeEvent),
    /// The reply to the latest join, accepted.
    Joined,
    /// A refused join answers here rather than closing the socket: the topic
    /// is simply never joined, which otherwise looks like a quiet server.
    Refused(String),
    /// The server let go of the topic and kept the socket: its token ran out
    /// (`phx_close`), the topic's process failed (`phx_error`), or something
    /// was sent to a topic already closed ("unmatched topic").
    Closed,
    Nothing,
}

/// The protocol, without the socket: which frames to send and what a received
/// one means. Kept apart so it can be tested without a server.
struct Protocol {
    topic: String,
    presence: Option<Value>,
    next_ref: u64,
    /// The ref of the latest join, so its reply can be told from the others'.
    join_ref: Option<String>,
}

impl Protocol {
    fn new(topic: &str, presence: Option<Value>) -> Self {
        Self {
            topic: format!("realtime:{topic}"),
            presence,
            next_ref: 0,
            join_ref: None,
        }
    }

    fn take_ref(&mut self) -> String {
        self.next_ref += 1;
        self.next_ref.to_string()
    }

    /// The topic is private, so the join carries the token the server checks
    /// `app.can_use_topic` with. Without `private` it would join a different,
    /// public topic of the same name and hear nothing.
    fn join(&mut self, token: &str) -> Value {
        let r = self.take_ref();
        self.join_ref = Some(r.clone());
        json!({
            "topic": self.topic,
            "event": "phx_join",
            "payload": {
                "config": {
                    "broadcast": { "self": false },
                    "presence": { "key": "", "enabled": self.presence.is_some() },
                    "private": true,
                },
                "access_token": token,
            },
            "ref": r,
        })
    }

    fn heartbeat(&mut self) -> Value {
        json!({ "topic": "phoenix", "event": "heartbeat", "payload": {}, "ref": self.take_ref() })
    }

    /// A bot's token expires within the hour and it signs in again; a topic
    /// still joined is handed the new one.
    fn access_token(&mut self, token: &str) -> Value {
        json!({
            "topic": self.topic,
            "event": "access_token",
            "payload": { "access_token": token },
            "ref": self.take_ref(),
        })
    }

    /// Tracked on every join, a rejoin included: the server forgets an entry
    /// when its socket drops, and joining again does not bring it back.
    fn track(&mut self) -> Option<Value> {
        let presence = self.presence.clone()?;
        Some(json!({
            "topic": self.topic,
            "event": "presence",
            "payload": { "type": "presence", "event": "track", "payload": presence },
            "ref": self.take_ref(),
        }))
    }

    fn read(&self, text: &str) -> Incoming {
        let Ok(frame) = serde_json::from_str::<Value>(text) else {
            return Incoming::Nothing;
        };
        if frame["topic"].as_str() != Some(&self.topic) {
            return Incoming::Nothing;
        }
        let payload = &frame["payload"];
        match frame["event"].as_str() {
            Some("broadcast") => match payload["event"].as_str() {
                Some(event) => Incoming::Broadcast(RealtimeEvent {
                    event: event.to_string(),
                    payload: match &payload["payload"] {
                        Value::Null => json!({}),
                        body => body.clone(),
                    },
                }),
                None => Incoming::Nothing,
            },
            Some("phx_close" | "phx_error") => Incoming::Closed,
            Some("phx_reply") => match payload["status"].as_str() {
                Some("ok") if frame["ref"].as_str() == self.join_ref.as_deref() => Incoming::Joined,
                Some("error") => match &payload["response"]["reason"] {
                    Value::String(reason) if reason == "unmatched topic" => Incoming::Closed,
                    Value::Null => Incoming::Refused("refused".to_string()),
                    Value::String(reason) => Incoming::Refused(reason.clone()),
                    reason => Incoming::Refused(reason.to_string()),
                },
                _ => Incoming::Nothing,
            },
            _ => Incoming::Nothing,
        }
    }
}

/// A single joined topic, kept open.
///
/// Reconnects on its own, forever: a bot whose connection dropped at three in
/// the morning should be listening again at three-oh-one. Dropping it, or
/// [`stop`](Self::stop), is the only thing that ends it.
pub struct RealtimeListener {
    token: watch::Sender<String>,
    connected: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl RealtimeListener {
    /// Start listening. What is heard comes out of the returned receiver.
    pub fn start(options: RealtimeOptions) -> (Self, mpsc::UnboundedReceiver<Heard>) {
        crate::tls::ensure_provider();
        let (out, heard) = mpsc::unbounded_channel();
        let (token, token_rx) = watch::channel(options.token.clone());
        let connected = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(run(options, token_rx, out, connected.clone()));
        (
            Self {
                token,
                connected,
                task,
            },
            heard,
        )
    }

    /// Whether the topic is joined right now.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Carry on with a new JWT. Sent to the topic if it is joined; a topic the
    /// server closed when the old one ran out is joined again with it.
    pub fn set_token(&self, token: &str) {
        self.token.send_replace(token.to_string());
    }

    pub fn stop(&self) {
        self.task.abort();
        self.connected.store(false, Ordering::Relaxed);
    }
}

impl Drop for RealtimeListener {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn socket_url(options: &RealtimeOptions) -> String {
    let base = match options.url.strip_prefix("http") {
        Some(rest) => format!("ws{rest}"),
        None => options.url.clone(),
    };
    let key: String = url::form_urlencoded::byte_serialize(options.anon_key.as_bytes()).collect();
    format!("{base}/realtime/v1/websocket?apikey={key}&vsn=1.0.0")
}

async fn run(
    options: RealtimeOptions,
    mut token: watch::Receiver<String>,
    out: mpsc::UnboundedSender<Heard>,
    connected: Arc<AtomicBool>,
) {
    let url = socket_url(&options);
    let mut attempt = 0usize;
    loop {
        let result = session(&options, &url, &mut token, &out, &connected, &mut attempt).await;
        connected.store(false, Ordering::Relaxed);
        if let Err(error) = result
            && out.send(Heard::Error(error)).is_err()
        {
            return; // Nobody is listening any more.
        }
        if out.is_closed() {
            return;
        }
        tokio::time::sleep(RETRY[attempt.min(RETRY.len() - 1)]).await;
        attempt += 1;
    }
}

/// One connection, from open to close. `Ok` is a socket the server closed.
async fn session(
    options: &RealtimeOptions,
    url: &str,
    token: &mut watch::Receiver<String>,
    out: &mpsc::UnboundedSender<Heard>,
    connected: &AtomicBool,
    attempt: &mut usize,
) -> Result<(), BotError> {
    let fail =
        |e: &dyn std::fmt::Display| BotError::new(format!("realtime: {}: {e}", options.topic));
    let (socket, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| fail(&e))?;
    let (mut write, mut read) = socket.split();
    *attempt = 0;

    let mut protocol = Protocol::new(&options.topic, options.presence.clone());
    let join = protocol.join(&token.borrow_and_update());
    write
        .send(Message::text(join.to_string()))
        .await
        .map_err(|e| fail(&e))?;

    // Whether the server accepted the latest join and has not let go since,
    // and when to join again after it did: the same backoff a dropped socket
    // waits, with whatever token is newest by then.
    let mut joined = false;
    let mut rejoin: Option<tokio::time::Instant> = None;
    let mut heartbeat =
        tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    loop {
        let frame = tokio::select! {
            frame = read.next() => frame,
            _ = heartbeat.tick() => {
                write.send(Message::text(protocol.heartbeat().to_string())).await.map_err(|e| fail(&e))?;
                continue;
            }
            _ = tokio::time::sleep_until(rejoin.unwrap_or_else(tokio::time::Instant::now)), if rejoin.is_some() => {
                rejoin = None;
                let join = protocol.join(&token.borrow_and_update());
                write.send(Message::text(join.to_string())).await.map_err(|e| fail(&e))?;
                continue;
            }
            changed = token.changed() => {
                if changed.is_err() {
                    return Ok(()); // The listener was dropped.
                }
                let token = token.borrow_and_update().clone();
                let message = if joined {
                    protocol.access_token(&token)
                } else {
                    rejoin = None;
                    protocol.join(&token)
                };
                write.send(Message::text(message.to_string())).await.map_err(|e| fail(&e))?;
                continue;
            }
        };
        let text = match frame {
            None => return Ok(()),
            Some(Err(e)) => return Err(fail(&e)),
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Close(_))) => return Ok(()),
            Some(Ok(_)) => continue,
        };
        match protocol.read(&text) {
            Incoming::Broadcast(event) => {
                if out.send(Heard::Event(event)).is_err() {
                    return Ok(());
                }
            }
            Incoming::Joined => {
                joined = true;
                *attempt = 0;
                connected.store(true, Ordering::Relaxed);
                if let Some(track) = protocol.track() {
                    write
                        .send(Message::text(track.to_string()))
                        .await
                        .map_err(|e| fail(&e))?;
                }
            }
            Incoming::Refused(reason) => {
                let _ = out.send(Heard::Error(BotError::new(format!(
                    "realtime: {}: {reason}",
                    options.topic
                ))));
            }
            Incoming::Closed => {
                joined = false;
                connected.store(false, Ordering::Relaxed);
                if rejoin.is_none() {
                    rejoin =
                        Some(tokio::time::Instant::now() + RETRY[(*attempt).min(RETRY.len() - 1)]);
                    *attempt += 1;
                }
            }
            Incoming::Nothing => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(v: Value) -> String {
        v.to_string()
    }

    #[test]
    fn the_join_is_private_and_carries_the_token() {
        let mut p = Protocol::new("user:abc", None);
        let join = p.join("jwt-1");
        assert_eq!(join["topic"], "realtime:user:abc");
        assert_eq!(join["event"], "phx_join");
        assert_eq!(join["payload"]["config"]["private"], true);
        assert_eq!(join["payload"]["config"]["presence"]["enabled"], false);
        assert_eq!(join["payload"]["access_token"], "jwt-1");
    }

    #[test]
    fn a_broadcast_arrives_as_its_event_and_its_ids() {
        let p = Protocol::new("user:abc", None);
        let heard = p.read(&frame(json!({
            "topic": "realtime:user:abc", "event": "broadcast",
            "payload": { "event": "new_message", "payload": { "id": 7 } },
        })));
        assert_eq!(
            heard,
            Incoming::Broadcast(RealtimeEvent {
                event: "new_message".into(),
                payload: json!({ "id": 7 })
            })
        );
    }

    #[test]
    fn a_frame_for_another_topic_or_garbage_is_nothing() {
        let p = Protocol::new("user:abc", None);
        let other = frame(json!({
            "topic": "realtime:user:xyz", "event": "broadcast",
            "payload": { "event": "new_message", "payload": {} },
        }));
        assert_eq!(p.read(&other), Incoming::Nothing);
        assert_eq!(p.read("not json"), Incoming::Nothing);
    }

    #[test]
    fn a_refused_join_is_reported() {
        let mut p = Protocol::new("user:abc", None);
        p.join("t");
        let heard = p.read(&frame(json!({
            "topic": "realtime:user:abc", "event": "phx_reply", "ref": "1",
            "payload": { "status": "error", "response": { "reason": "Unauthorized" } },
        })));
        assert_eq!(heard, Incoming::Refused("Unauthorized".into()));
    }

    #[test]
    fn presence_is_tracked_off_the_join_reply_and_nothing_else() {
        let mut p = Protocol::new("presence:s", Some(json!({ "userId": "u" })));
        let join = p.join("t");
        assert_eq!(join["payload"]["config"]["presence"]["enabled"], true);
        let hb = p.heartbeat();
        let reply = |r: &Value| {
            frame(json!({
                "topic": "realtime:presence:s", "event": "phx_reply", "ref": r,
                "payload": { "status": "ok", "response": {} },
            }))
        };
        assert_eq!(p.read(&reply(&hb["ref"])), Incoming::Nothing);
        assert_eq!(p.read(&reply(&join["ref"])), Incoming::Joined);
        let track = p.track().unwrap();
        assert_eq!(track["payload"]["event"], "track");
        assert_eq!(track["payload"]["payload"]["userId"], "u");
    }

    /// Realtime closes a topic the second its token runs out ("Token has
    /// expired", then `phx_close`) and keeps the socket; anything sent to the
    /// topic after that is answered "unmatched topic". All three mean the
    /// topic is gone, not that something was refused.
    #[test]
    fn a_topic_the_server_let_go_of_reads_as_closed() {
        let p = Protocol::new("user:abc", None);
        let on_topic = |event: &str, payload: Value| {
            frame(
                json!({ "topic": "realtime:user:abc", "event": event, "ref": null, "payload": payload }),
            )
        };
        assert_eq!(p.read(&on_topic("phx_close", json!({}))), Incoming::Closed);
        assert_eq!(p.read(&on_topic("phx_error", json!({}))), Incoming::Closed);
        assert_eq!(
            p.read(&on_topic(
                "phx_reply",
                json!({ "status": "error", "response": { "reason": "unmatched topic" } })
            )),
            Incoming::Closed
        );
    }

    #[test]
    fn a_listener_without_presence_never_tracks() {
        assert!(Protocol::new("user:abc", None).track().is_none());
    }

    #[test]
    fn the_socket_url_swaps_the_scheme_and_escapes_the_key() {
        let options = RealtimeOptions {
            url: "https://rift.example".into(),
            anon_key: "a+b/c".into(),
            token: String::new(),
            topic: "t".into(),
            presence: None,
        };
        assert_eq!(
            socket_url(&options),
            "wss://rift.example/realtime/v1/websocket?apikey=a%2Bb%2Fc&vsn=1.0.0"
        );
    }

    /// The whole loop against a real socket: the join goes out, a broadcast
    /// comes back, a new token reaches the joined topic, and a dropped
    /// connection is joined again with the newest token.
    #[tokio::test]
    async fn it_joins_hears_renews_and_rejoins() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, mut seen) = mpsc::unbounded_channel::<Value>();

        tokio::spawn(async move {
            for round in 0..2 {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
                let join: Value =
                    serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                        .unwrap();
                seen_tx.send(join.clone()).unwrap();
                if round == 0 {
                    let ok = json!({
                        "topic": "realtime:user:abc", "event": "phx_reply", "ref": join["ref"],
                        "payload": { "status": "ok", "response": {} },
                    });
                    ws.send(Message::text(ok.to_string())).await.unwrap();
                    let broadcast = json!({
                        "topic": "realtime:user:abc", "event": "broadcast",
                        "payload": { "event": "new_message", "payload": { "id": 1 } },
                    });
                    ws.send(Message::text(broadcast.to_string())).await.unwrap();
                    let renewed: Value =
                        serde_json::from_str(ws.next().await.unwrap().unwrap().to_text().unwrap())
                            .unwrap();
                    seen_tx.send(renewed).unwrap();
                    // Drop the connection: the listener has to come back.
                }
            }
        });

        let (rt, mut heard) = RealtimeListener::start(RealtimeOptions {
            url: format!("http://127.0.0.1:{port}"),
            anon_key: "anon".into(),
            token: "old".into(),
            topic: "user:abc".into(),
            presence: None,
        });

        let join = seen.recv().await.unwrap();
        assert_eq!(join["payload"]["access_token"], "old");
        match heard.recv().await.unwrap() {
            Heard::Event(e) => assert_eq!(e.payload["id"], 1),
            other => panic!("{other:?}"),
        }
        rt.set_token("new");
        let renewed = seen.recv().await.unwrap();
        assert_eq!(renewed["event"], "access_token");
        assert_eq!(renewed["payload"]["access_token"], "new");

        let rejoin = tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rejoin["event"], "phx_join");
        assert_eq!(rejoin["payload"]["access_token"], "new");
        rt.stop();
    }

    /// What took a live bot offline an hour after every start: the server
    /// closed its topic when the token ran out, kept the socket, and nothing
    /// joined the topic again. Here it is joined again on the same socket,
    /// and a token renewed while it is closed goes out as a join.
    #[tokio::test]
    async fn a_topic_the_server_closes_is_joined_again() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, mut seen) = mpsc::unbounded_channel::<Value>();

        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            let topic = "realtime:user:abc";
            let mut close = true;
            while let Some(Ok(message)) = ws.next().await {
                let Ok(text) = message.to_text() else {
                    continue;
                };
                let frame: Value = serde_json::from_str(text).unwrap();
                if frame["event"] == "heartbeat" {
                    continue;
                }
                seen_tx.send(frame.clone()).unwrap();
                if frame["event"] != "phx_join" {
                    continue;
                }
                let ok = json!({ "topic": topic, "event": "phx_reply", "ref": frame["ref"],
                                 "payload": { "status": "ok", "response": {} } });
                ws.send(Message::text(ok.to_string())).await.unwrap();
                if close {
                    close = false;
                    for event in ["system", "phx_close"] {
                        let f =
                            json!({ "topic": topic, "event": event, "ref": null, "payload": {} });
                        ws.send(Message::text(f.to_string())).await.unwrap();
                    }
                }
            }
        });

        let (rt, _heard) = RealtimeListener::start(RealtimeOptions {
            url: format!("http://127.0.0.1:{port}"),
            anon_key: "anon".into(),
            token: "old".into(),
            topic: "user:abc".into(),
            presence: None,
        });
        let next = async |seen: &mut mpsc::UnboundedReceiver<Value>| {
            tokio::time::timeout(Duration::from_secs(5), seen.recv())
                .await
                .expect("nothing sent")
                .unwrap()
        };

        assert_eq!(next(&mut seen).await["event"], "phx_join");
        let rejoin = next(&mut seen).await;
        assert_eq!(
            rejoin["event"], "phx_join",
            "joined again, on the same socket"
        );
        assert_eq!(rejoin["payload"]["access_token"], "old");

        // Joined again: a renewed token now goes out as an update.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(rt.connected());
        rt.set_token("new");
        let renewed = next(&mut seen).await;
        assert_eq!(renewed["event"], "access_token");
        assert_eq!(renewed["payload"]["access_token"], "new");
        rt.stop();
    }
}

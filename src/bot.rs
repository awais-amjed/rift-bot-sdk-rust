//! A running bot: hear what it is addressed, answer it.
//!
//! **It only ever sees what it was addressed.** That is not this module being
//! careful: `messages_select` will not return anything else, so a bug here
//! cannot widen it, and neither can a bug in a bot.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::channel::{ChannelMessage, ChannelReader};
use crate::crypto::{b64, random_bytes, signed_payload};
use crate::dm::{DirectMessage, DirectMessages};
use crate::error::{BotError, Result};
use crate::image::image_info;
use crate::panel::Block;
use crate::realtime::{Heard, RealtimeListener, RealtimeOptions};
use crate::session::BotSession;
use crate::suggest::{
    SUGGEST_EVENT, SUGGESTIONS_EVENT, SuggestRequest, Suggestion, parse_suggest_request,
    suggestions_payload,
};

/// One command, or one press, as the bot receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotMessage {
    pub id: i64,
    pub channel_id: String,
    pub sender_id: String,
    /// The whole line typed, slash included. The SDK does not split arguments,
    /// because only the bot knows what its arguments mean.
    pub text: String,
    /// Set when this is a press on a panel: the button's own action id.
    pub action_id: Option<String>,
    /// The chosen option's value, for a menu. `None` for a button.
    pub action_value: Option<String>,
    /// The panel that was pressed, for editing it back.
    pub panel_id: Option<i64>,
}

impl BotMessage {
    /// Whether somebody pressed something rather than typed something.
    pub fn is_action(&self) -> bool {
        self.action_id.is_some()
    }

    /// The verb, lower-cased and without the slash: `/Play a song` → `play`.
    pub fn command(&self) -> String {
        let body = self.text.trim();
        let Some(rest) = body.strip_prefix('/') else {
            return String::new();
        };
        rest.split(char::is_whitespace)
            .next()
            .unwrap_or("")
            .to_lowercase()
    }

    /// Everything after the verb, trimmed. Empty for a bare command.
    pub fn args(&self) -> &str {
        let body = self.text.trim();
        body.find(char::is_whitespace)
            .map_or("", |at| body[at..].trim())
    }
}

/// Who a panel shows to, and whether it is the bot's current one.
#[derive(Debug, Clone, Default)]
pub struct PanelOptions {
    /// A member's user id: only they see the panel. Check a press on it came
    /// from them all the same: nothing stops another member naming a panel id
    /// they cannot see.
    pub only_for: Option<String>,
    /// This is the bot's current panel, a player while it plays. Rift draws it
    /// in the bar above the channel's message box rather than in the history,
    /// while the bot is connected, so it does not scroll away. Redraw with
    /// [`Bot::edit_panel_live`] to keep it there. WIRE.md §5.
    pub live: bool,
}

/// A panel row's `blocks` column: `live` only when it is.
fn panel_blocks(blocks: &[Block], live: bool) -> Value {
    if live {
        json!({ "v": 1, "live": true, "blocks": blocks })
    } else {
        json!({ "v": 1, "blocks": blocks })
    }
}

/// What a handler may return: nothing, or a result whose error is reported
/// through [`Bot::on_error`], so `?` works inside one.
pub trait HandlerOutput: Send {
    fn into_error(self) -> Option<BotError>;
}

impl HandlerOutput for () {
    fn into_error(self) -> Option<BotError> {
        None
    }
}

impl<E: std::fmt::Display + Send> HandlerOutput for std::result::Result<(), E> {
    fn into_error(self) -> Option<BotError> {
        self.err().map(|e| BotError::new(e.to_string()))
    }
}

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;
type Handler<T> = Arc<dyn Fn(T) -> BoxFuture<Option<BotError>> + Send + Sync>;
type SuggestHandler = Arc<dyn Fn(SuggestRequest) -> BoxFuture<Vec<Suggestion>> + Send + Sync>;
type ErrorHandler = Arc<dyn Fn(BotError) + Send + Sync>;

fn boxed<T, F, Fut>(handler: F) -> Handler<T>
where
    F: Fn(T) -> Fut + Send + Sync + 'static,
    Fut: Future + Send + 'static,
    Fut::Output: HandlerOutput,
{
    Arc::new(move |value| {
        let fut = handler(value);
        Box::pin(async move { fut.await.into_error() })
    })
}

struct Watch {
    reader: ChannelReader,
    last_seen: i64,
    handler: Handler<ChannelMessage>,
}

/// Cursors, one per source, advanced by what was delivered. Behind one async
/// lock, which is also what keeps two drains from running at once: a slow
/// handler would otherwise let two passes read the same rows before either
/// advanced, and a button pressed once would count twice.
#[derive(Default)]
struct Cursors {
    last_seen: i64,
    last_dm: i64,
    watched: Vec<Watch>,
}

#[derive(Default)]
struct Running {
    tasks: Vec<JoinHandle<()>>,
    realtime: Option<RealtimeListener>,
    presence: Option<RealtimeListener>,
    /// Set while a drain is queued from a broadcast, so a burst of them costs
    /// one pass rather than one each.
    queued: bool,
}

struct Inner {
    session: BotSession,
    dms: DirectMessages,
    poll: Duration,
    use_realtime: bool,
    cursors: tokio::sync::Mutex<Cursors>,
    running: Mutex<Running>,
    on_message: Mutex<Option<Handler<BotMessage>>>,
    on_dm: Mutex<Option<Handler<DirectMessage>>>,
    on_suggest: Mutex<Option<SuggestHandler>>,
    on_error: Mutex<Option<ErrorHandler>>,
    /// Per asker: whether a suggestion is being worked out, and the newest
    /// request that came in meanwhile. Only the last is worth answering.
    suggesting: Mutex<HashMap<String, Option<SuggestRequest>>>,
    stopped: watch::Sender<bool>,
}

/// A running bot. Cheap to clone; clones are the same bot, which is how a
/// handler gets hold of it to answer.
#[derive(Clone)]
pub struct Bot {
    inner: Arc<Inner>,
}

/// How often the backstop poll runs with Realtime on. Not how messages
/// normally arrive, which is why it is half a minute.
pub const DEFAULT_POLL: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct CommandRow {
    id: i64,
    channel_id: String,
    sender_id: String,
    ciphertext: Option<String>,
    action_id: Option<String>,
    action_value: Option<String>,
    reply_to: Option<i64>,
}

#[derive(Deserialize)]
struct IdRow {
    id: i64,
}

impl Bot {
    /// A bot that hears the server over Realtime, with a slow poll behind it.
    pub fn new(session: BotSession) -> Self {
        Self::with_options(session, DEFAULT_POLL, true)
    }

    /// `poll` is the backstop, not the delivery, while `realtime` is on: the
    /// poll catches what a dropped frame lost. With `realtime` off the poll is
    /// all there is, and a short one is what you want.
    pub fn with_options(session: BotSession, poll: Duration, realtime: bool) -> Self {
        let (stopped, _) = watch::channel(false);
        Self {
            inner: Arc::new(Inner {
                dms: DirectMessages::new(session.clone()),
                session,
                poll,
                use_realtime: realtime,
                cursors: tokio::sync::Mutex::new(Cursors::default()),
                running: Mutex::new(Running::default()),
                on_message: Mutex::new(None),
                on_dm: Mutex::new(None),
                on_suggest: Mutex::new(None),
                on_error: Mutex::new(None),
                suggesting: Mutex::new(HashMap::new()),
                stopped,
            }),
        }
    }

    pub fn session(&self) -> &BotSession {
        &self.inner.session
    }

    /// Direct messages, the one thing a bot both opens and seals.
    pub fn dms(&self) -> &DirectMessages {
        &self.inner.dms
    }

    /// Answer direct messages too. Call before [`listen`](Self::listen).
    ///
    /// A DM to a bot is private from the *server* as well as from every member
    /// (BOTS.md §3): sealed to the two of you and nothing else can read it.
    pub fn on_direct_message<F, Fut>(&self, handler: F)
    where
        F: Fn(DirectMessage) -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: HandlerOutput,
    {
        *self.inner.on_dm.lock().unwrap() = Some(boxed(handler));
    }

    /// Offer suggestions while somebody types one of this bot's commands, for
    /// a command the manifest marks `"suggest": true`. Return up to ten; picking
    /// one sends `/<command> <value>`, which then arrives like anything typed.
    ///
    /// **Look things up; do nothing.** Who asked is the asking client's claim
    /// (WIRE.md §7), so a handler that queued a song would do it for anybody
    /// who said they were somebody. Needs Realtime. Call before `listen`.
    pub fn on_suggest<F, Fut>(&self, handler: F)
    where
        F: Fn(SuggestRequest) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Vec<Suggestion>> + Send + 'static,
    {
        *self.inner.on_suggest.lock().unwrap() =
            Some(Arc::new(move |request| Box::pin(handler(request))));
    }

    /// Hear about a pass that failed on its merits, or a handler's error. Not
    /// an expired session, which is recovered silently. Without a handler they
    /// go to the `log` crate at `warn`.
    pub fn on_error(&self, handler: impl Fn(BotError) + Send + Sync + 'static) {
        *self.inner.on_error.lock().unwrap() = Some(Arc::new(handler));
    }

    fn report(&self, error: BotError) {
        let handler = self.inner.on_error.lock().unwrap().clone();
        match handler {
            Some(handler) => handler(error),
            None => log::warn!("[rift] {error}"),
        }
    }

    /// Read a channel this bot has been **granted** (BOTS.md §6), the one
    /// place a bot sees what it was not addressed. Call before `listen`.
    ///
    /// Returns the grant's `from_key_version`, or `None` when this bot was
    /// never granted this channel, which is worth checking: an ungranted bot
    /// polls forever and receives nothing, and that looks like a quiet channel.
    /// Reading starts from now.
    pub async fn watch_channel<F, Fut>(&self, channel_id: &str, handler: F) -> Result<Option<i64>>
    where
        F: Fn(ChannelMessage) -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: HandlerOutput,
    {
        if self.inner.session.user_id().is_none() {
            self.inner.session.login().await?;
        }
        let reader = ChannelReader::new(self.inner.session.clone(), channel_id);
        let Some(granted_from) = reader.granted_from().await? else {
            return Ok(None);
        };
        let last_seen = reader.newest_id().await?;
        self.inner.cursors.lock().await.watched.push(Watch {
            reader,
            last_seen,
            handler: boxed(handler),
        });
        Ok(Some(granted_from))
    }

    /// Start answering: `handler` is called once per command or press, in id
    /// order. Returns once listening; [`wait`](Self::wait) for the end.
    ///
    /// Starts from *now*: a bot restarting does not replay a backlog of
    /// commands people gave up on minutes ago. What arrives over Realtime is
    /// ids, so every read still goes through the same queries and policies.
    pub async fn listen<F, Fut>(&self, handler: F) -> Result<()>
    where
        F: Fn(BotMessage) -> Fut + Send + Sync + 'static,
        Fut: Future + Send + 'static,
        Fut::Output: HandlerOutput,
    {
        *self.inner.on_message.lock().unwrap() = Some(boxed(handler));
        let session = &self.inner.session;
        session.login().await?;
        // So members can seal this bot things: today, its media key for an
        // encrypted call.
        session.publish_chat_key().await?;
        {
            let mut cursors = self.inner.cursors.lock().await;
            cursors.last_seen = self.newest_command_id().await?;
            if self.inner.on_dm.lock().unwrap().is_some() {
                cursors.last_dm = self.inner.dms.newest_id().await?;
            }
        }

        let bot = self.clone();
        let poll = self.inner.poll;
        let poller = tokio::spawn(async move {
            let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + poll, poll);
            loop {
                interval.tick().await;
                bot.drain().await;
            }
        });
        self.inner.running.lock().unwrap().tasks.push(poller);

        if self.inner.use_realtime {
            self.start_realtime();
            self.start_presence().await?;
        }
        Ok(())
    }

    /// Resolves once [`stop`](Self::stop) is called. What a bot's `main`
    /// awaits after `listen`.
    pub async fn wait(&self) {
        let mut stopped = self.inner.stopped.subscribe();
        let _ = stopped.wait_for(|s| *s).await;
    }

    /// Sign in again after the session expired, and hand the new token to the
    /// realtime sockets.
    ///
    /// The poll does this itself on a 401. Call it when one of *your* calls
    /// gets one (a panel redraw an hour into a song) and retry the call.
    /// `session().login()` alone would leave the sockets on the old token,
    /// and the server closes a topic whose token runs out.
    pub async fn renew_session(&self) -> Result<()> {
        self.inner.session.login().await?;
        let token = self.inner.session.token().unwrap_or_default();
        let running = self.inner.running.lock().unwrap();
        for listener in running.realtime.iter().chain(running.presence.iter()) {
            listener.set_token(&token);
        }
        Ok(())
    }

    pub fn stop(&self) {
        let mut running = self.inner.running.lock().unwrap();
        for task in running.tasks.drain(..) {
            task.abort();
        }
        running.realtime = None;
        running.presence = None;
        self.inner.stopped.send_replace(true);
    }

    /// Whether the bot is hearing the server rather than asking it.
    pub fn listening(&self) -> bool {
        self.inner
            .running
            .lock()
            .unwrap()
            .realtime
            .as_ref()
            .is_some_and(RealtimeListener::connected)
    }

    fn start_realtime(&self) {
        let Some(user_id) = self.inner.session.user_id() else {
            return;
        };
        let (listener, mut heard) =
            RealtimeListener::start(self.realtime_options(format!("user:{user_id}"), None));
        let bot = self.clone();
        let task = tokio::spawn(async move {
            while let Some(heard) = heard.recv().await {
                match heard {
                    // A request for suggestions carries what it asks and reads
                    // nothing. Every other event means "something you can read
                    // changed", and one drain answers all of them.
                    Heard::Event(event) if event.event == SUGGEST_EVENT => {
                        bot.suggest(&event.payload)
                    }
                    Heard::Event(_) => bot.soon(),
                    Heard::Error(error) => bot.report(error),
                }
            }
        });
        let mut running = self.inner.running.lock().unwrap();
        running.realtime = Some(listener);
        running.tasks.push(task);
    }

    /// Be online: announce this bot on the server's presence topic, the entry
    /// a member's app reads `userId` and `displayName` from.
    async fn start_presence(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct NameRow {
            display_name: Option<String>,
        }
        let user_id = self.inner.session.require_user_id()?;
        let rows: Vec<NameRow> = self
            .inner
            .session
            .select(&format!("users?select=display_name&id=eq.{user_id}"))
            .await?;
        let display_name = rows
            .into_iter()
            .next()
            .and_then(|r| r.display_name)
            .unwrap_or_default();
        let topic = format!("presence:{}", self.inner.session.server_id());
        let presence = json!({ "userId": user_id, "displayName": display_name });
        let (listener, mut heard) =
            RealtimeListener::start(self.realtime_options(topic, Some(presence)));
        let bot = self.clone();
        let task = tokio::spawn(async move {
            while let Some(heard) = heard.recv().await {
                if let Heard::Error(error) = heard {
                    bot.report(error);
                }
            }
        });
        let mut running = self.inner.running.lock().unwrap();
        running.presence = Some(listener);
        running.tasks.push(task);
        Ok(())
    }

    fn realtime_options(&self, topic: String, presence: Option<Value>) -> RealtimeOptions {
        let session = &self.inner.session;
        RealtimeOptions {
            url: session.url().to_string(),
            anon_key: session.anon_key(),
            token: session.token().unwrap_or_default(),
            topic,
            presence,
        }
    }

    /// Coalesce a burst of announcements (a message, its reaction, an edit)
    /// into one pass, a moment from now.
    fn soon(&self) {
        {
            let mut running = self.inner.running.lock().unwrap();
            if running.queued {
                return;
            }
            running.queued = true;
        }
        let bot = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            bot.inner.running.lock().unwrap().queued = false;
            bot.drain().await;
        });
    }

    fn suggest(&self, payload: &Value) {
        let Some(request) = parse_suggest_request(payload) else {
            return;
        };
        if self.inner.on_suggest.lock().unwrap().is_none() {
            return;
        }
        {
            let mut suggesting = self.inner.suggesting.lock().unwrap();
            if let Some(waiting) = suggesting.get_mut(&request.from) {
                *waiting = Some(request);
                return;
            }
            suggesting.insert(request.from.clone(), None);
        }
        let bot = self.clone();
        tokio::spawn(async move { bot.answer(request).await });
    }

    async fn answer(&self, mut request: SuggestRequest) {
        loop {
            let handler = self.inner.on_suggest.lock().unwrap().clone();
            if let Some(handler) = handler {
                let items = handler(request.clone()).await;
                let bot_id = self.inner.session.user_id().unwrap_or_default();
                let payload = suggestions_payload(&request, &bot_id, &items);
                let topic = format!("user:{}", request.from);
                if let Err(error) = self
                    .inner
                    .session
                    .broadcast(&topic, SUGGESTIONS_EVENT, &payload)
                    .await
                {
                    self.report(BotError::new(format!(
                        "suggestions for /{}: {error}",
                        request.command
                    )));
                }
            }
            let mut suggesting = self.inner.suggesting.lock().unwrap();
            match suggesting.get_mut(&request.from).and_then(Option::take) {
                Some(next) => request = next,
                None => {
                    suggesting.remove(&request.from);
                    return;
                }
            }
        }
    }

    /// One pass over everything this bot can hear. Skipped, not queued, while
    /// another pass is running: that one reads the same rows.
    async fn drain(&self) {
        let Ok(mut cursors) = self.inner.cursors.try_lock() else {
            return;
        };
        let Err(error) = self.drain_with(&mut cursors).await else {
            return;
        };
        drop(cursors);
        // A session expires an hour after it is minted, and the only recovery
        // needs no state: sign again with the key the seed derives. 401 and
        // nothing else: a 403 is the database refusing on the merits, and an
        // error with no status is this SDK's own. Sending those round the
        // login path is how a bot that could not do what it was asked looked
        // exactly like a bot nobody had asked.
        if error.status == Some(401) {
            if let Err(error) = self.renew_session().await {
                self.report(error);
            }
            return;
        }
        self.report(error);
    }

    async fn drain_with(&self, cursors: &mut Cursors) -> Result<()> {
        let me = self.inner.session.require_user_id()?;
        let rows: Vec<CommandRow> = self
            .inner
            .session
            .select(&format!(
                "messages?select=id,channel_id,sender_id,ciphertext,action_id,action_value,reply_to\
                 &to_bot=eq.{me}&id=gt.{}&order=id.asc",
                cursors.last_seen
            ))
            .await?;
        let handler = self.inner.on_message.lock().unwrap().clone();
        for row in rows {
            cursors.last_seen = row.id;
            let message = BotMessage {
                id: row.id,
                channel_id: row.channel_id,
                sender_id: row.sender_id,
                text: row.ciphertext.unwrap_or_default(),
                action_id: row.action_id,
                action_value: row.action_value,
                panel_id: row.reply_to,
            };
            if let Some(handler) = &handler
                && let Some(error) = handler(message).await
            {
                self.report(error);
            }
        }

        // The same pass, not a second timer: one cursor per source, but one
        // drain, so a slow handler cannot let them overlap.
        let dm_handler = self.inner.on_dm.lock().unwrap().clone();
        if let Some(handler) = dm_handler {
            for dm in self.inner.dms.since(cursors.last_dm).await? {
                cursors.last_dm = dm.id;
                if let Some(error) = handler(dm).await {
                    self.report(error);
                }
            }
        }

        for watch in &mut cursors.watched {
            // Advanced by what came back, not by the newest row: `since` stops
            // at a message whose key has not been sealed yet, and jumping past
            // it would drop that stretch of conversation for good.
            for message in watch.reader.since(watch.last_seen).await? {
                watch.last_seen = message.id;
                if let Some(error) = (watch.handler)(message).await {
                    self.report(error);
                }
            }
        }
        Ok(())
    }

    async fn newest_command_id(&self) -> Result<i64> {
        let me = self.inner.session.require_user_id()?;
        let rows: Vec<IdRow> = self
            .inner
            .session
            .select(&format!(
                "messages?select=id&to_bot=eq.{me}&order=id.desc&limit=1"
            ))
            .await?;
        Ok(rows.first().map_or(0, |r| r.id))
    }

    // ── Writing ──────────────────────────────────────────────────────

    /// Signed but not sealed, which is the whole shape of a bot's message.
    ///
    /// Not sealed: a bot holds no channel key to seal with. Signed: the reply
    /// carries the bot's name in a room full of people, and clients drop what
    /// they cannot verify.
    fn envelope(&self, channel_id: &str, text: &str) -> Value {
        json!({
            "channel_id": channel_id,
            "ciphertext": text,
            "nonce": "",
            "key_version": 0,
            "signature": self.inner.session.identity().sign(&signed_payload(channel_id, 0, "", text)),
        })
    }

    /// Answer in the channel, where everybody can see it.
    pub async fn reply(&self, to: &BotMessage, text: &str) -> Result<()> {
        self.post_reply(to, text, None).await
    }

    /// Answer only the person who asked. Private from the *channel* (no member
    /// ever receives the row) and not from the server, which stores it in the
    /// clear like everything else a bot writes.
    pub async fn reply_privately(&self, to: &BotMessage, text: &str) -> Result<()> {
        self.post_reply(to, text, Some(&to.sender_id)).await
    }

    async fn post_reply(&self, to: &BotMessage, text: &str, only_for: Option<&str>) -> Result<()> {
        let mut row = self.envelope(&to.channel_id, text);
        row["reply_to"] = json!(to.id);
        if let Some(member) = only_for {
            row["ephemeral_for"] = json!(member);
        }
        self.inner.session.insert("messages", &row).await
    }

    /// Say something in a channel that nobody asked for: a watching bot
    /// noticed something rather than being addressed.
    pub async fn post(&self, channel_id: &str, text: &str) -> Result<()> {
        self.inner
            .session
            .insert("messages", &self.envelope(channel_id, text))
            .await
    }

    /// Post a panel, and return its id. Hold on to it: a queue that posts a
    /// new panel per track is the log a panel exists to replace.
    pub async fn panel(
        &self,
        channel_id: &str,
        blocks: &[Block],
        options: &PanelOptions,
    ) -> Result<Option<i64>> {
        // Signed over the empty body, not the blocks: the signature attests
        // who wrote the row, and the panel is structure the client validates
        // itself. Signing a JSON encoding would make a whitespace change break
        // every panel.
        let mut row = self.envelope(channel_id, "");
        row["blocks"] = panel_blocks(blocks, options.live);
        if let Some(member) = &options.only_for {
            row["ephemeral_for"] = json!(member);
        }
        let rows: Vec<IdRow> = self
            .inner
            .session
            .insert_returning("messages", &row)
            .await?;
        Ok(rows.first().map(|r| r.id))
    }

    /// Redraw a panel in place. Only the bot that posted it may; the database
    /// announces the edit to everyone who may see the channel.
    ///
    /// The redraw is not live: a live panel redrawn here leaves the bar and is
    /// back in the history. Use [`Bot::edit_panel_live`] to keep it there.
    pub async fn edit_panel(&self, panel_id: i64, blocks: &[Block]) -> Result<()> {
        self.edit_panel_live(panel_id, blocks, false).await
    }

    /// Redraw a panel in place, saying whether it is still the bot's current
    /// one ([`PanelOptions::live`]). Pass `true` on every redraw of a player
    /// while it plays, and `false` on its last.
    pub async fn edit_panel_live(&self, panel_id: i64, blocks: &[Block], live: bool) -> Result<()> {
        self.inner
            .session
            .patch(
                &format!("messages?id=eq.{panel_id}"),
                &json!({ "blocks": panel_blocks(blocks, live) }),
            )
            .await
    }

    /// Take a panel away, for one whose job is done.
    pub async fn delete_panel(&self, panel_id: i64) -> Result<()> {
        self.inner
            .session
            .remove(&format!("messages?id=eq.{panel_id}"))
            .await
    }

    /// Store a picture for a panel in `channel_id`, and get the block that
    /// shows it.
    ///
    /// It goes to this server's own attachment bucket, under the channel,
    /// never a URL, so no member's client fetches from wherever a URL pointed
    /// (BOTS.md §5). **Stored unencrypted**, like the panel. PNG, JPEG, WebP or
    /// GIF. Every call stores a new file: [`delete_image`](Self::delete_image)
    /// the old one when a panel moves on.
    pub async fn upload_image(
        &self,
        channel_id: &str,
        bytes: Vec<u8>,
        alt: Option<&str>,
    ) -> Result<Block> {
        let info = image_info(&bytes)
            .ok_or_else(|| BotError::new("upload_image: not a PNG, JPEG, WebP or GIF"))?;
        let name: String = random_bytes::<16>()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = format!("{channel_id}/{name}.{}", info.extension);
        let sha256 = b64(&Sha256::digest(&bytes));
        self.inner
            .session
            .upload_object(&self.bucket(), &path, bytes, info.content_type)
            .await?;
        let mut block = json!({ "type": "image", "path": path, "sha256": sha256 });
        if let Some((width, height)) = info.size {
            block["width"] = json!(width);
            block["height"] = json!(height);
        }
        if let Some(alt) = alt {
            block["text"] = json!(alt);
        }
        Ok(block)
    }

    /// Delete a picture [`upload_image`](Self::upload_image) stored, by the
    /// block's `path`.
    pub async fn delete_image(&self, path: &str) -> Result<()> {
        self.inner.session.remove_object(&self.bucket(), path).await
    }

    /// The server's attachment bucket: one per server, `chat-<server id>`.
    fn bucket(&self) -> String {
        format!("chat-{}", self.inner.session.server_id())
    }

    /// The voice channels this bot has been summoned to, newest first, with
    /// who summoned it.
    ///
    /// A bot cannot see a voice channel it was not asked into, so this row is
    /// the only way it learns the id. The usual shape: take the command, read
    /// this, join the newest. Empty means nobody has asked.
    pub async fn summons(&self) -> Result<Vec<(String, Option<String>)>> {
        #[derive(Deserialize)]
        struct Row {
            channel_id: String,
            summoned_by: Option<String>,
        }
        let me = self.inner.session.require_user_id()?;
        let rows: Vec<Row> = self
            .inner
            .session
            .select(&format!(
                "bot_voice_summons?select=channel_id,summoned_by&bot_id=eq.{me}&order=summoned_at.desc"
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| (r.channel_id, r.summoned_by))
            .collect())
    }

    /// Join a voice channel, and get something to play through.
    ///
    /// **The bot publishes; it does not hear**, unless an admin granted it
    /// listening on this channel: check
    /// [`can_hear`](crate::voice::VoiceConnection::can_hear) rather than
    /// wondering why nothing arrives.
    #[cfg(feature = "voice")]
    pub async fn join_voice(
        &self,
        channel_id: &str,
        options: &crate::voice::VoiceOptions,
    ) -> Result<crate::voice::VoiceConnection> {
        crate::voice::join_voice(&self.inner.session, channel_id, options).await
    }

    /// Leave, and give back the welcome. Dropping the summon drops the media
    /// key with it; a row left behind is a bot that looks present and is not.
    pub async fn dismiss_self(&self, channel_id: &str) -> Result<()> {
        let me = self.inner.session.require_user_id()?;
        self.inner
            .session
            .rpc::<Value>(
                "dismiss_bot_from_voice",
                &json!({ "p_bot": me, "p_channel": channel_id }),
            )
            .await
            .map(drop)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::crypto::verify_signature;
    use crate::test_server::TestServer;

    fn message(text: &str) -> BotMessage {
        BotMessage {
            id: 1,
            channel_id: "c".into(),
            sender_id: "s".into(),
            text: text.into(),
            action_id: None,
            action_value: None,
            panel_id: None,
        }
    }

    #[test]
    fn the_verb_and_its_arguments() {
        assert_eq!(message("/Play  a song ").command(), "play");
        assert_eq!(message("/Play  a song ").args(), "a song");
        assert_eq!(message("/skip").args(), "");
        assert_eq!(message("hello").command(), "");
    }

    /// A reply is signed over `chatmsg:v1:<channel>:0::<text>`, with the empty
    /// nonce between two colons, or nobody ever sees it.
    #[tokio::test]
    async fn a_reply_is_signed_but_not_sealed() {
        let inserted = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = inserted.clone();
        let server = TestServer::start(move |req| {
            seen.lock().unwrap().push(req.body.clone());
            (201, json!(null))
        })
        .await;
        let bot = Bot::new(server.session("bot"));
        let mut to = message("/echo hi");
        to.id = 42;
        bot.reply_privately(&to, "hi").await.unwrap();

        let row = inserted.lock().unwrap()[0].clone();
        assert_eq!(
            (row["nonce"].as_str(), row["key_version"].as_i64()),
            (Some(""), Some(0))
        );
        assert_eq!(
            (row["reply_to"].as_i64(), row["ephemeral_for"].as_str()),
            (Some(42), Some("s"))
        );
        assert!(verify_signature(
            "chatmsg:v1:c:0::hi",
            row["signature"].as_str().unwrap(),
            &bot.session().identity().public_key
        ));
    }

    /// The panel rows a bot writes, as the server sees them.
    async fn panel_rows(write: impl AsyncFnOnce(&Bot)) -> Vec<Value> {
        let written = Arc::new(Mutex::new(Vec::<Value>::new()));
        let seen = written.clone();
        let server = TestServer::start(move |req| {
            seen.lock().unwrap().push(req.body.clone());
            (201, json!([{ "id": 7 }]))
        })
        .await;
        let bot = Bot::new(server.session("bot"));
        write(&bot).await;
        written.lock().unwrap().clone()
    }

    #[tokio::test]
    async fn a_live_panel_is_written_with_live_true() {
        let blocks = [crate::panel::heading("Now playing")];
        let rows = panel_rows(async |bot: &Bot| {
            let live = PanelOptions {
                live: true,
                ..Default::default()
            };
            bot.panel("c", &blocks, &live).await.unwrap();
            bot.edit_panel_live(7, &blocks, true).await.unwrap();
        })
        .await;
        assert_eq!(
            rows[0]["blocks"],
            json!({ "v": 1, "live": true, "blocks": blocks })
        );
        assert_eq!(
            rows[1],
            json!({ "blocks": { "v": 1, "live": true, "blocks": blocks } })
        );
    }

    #[tokio::test]
    async fn an_edit_without_live_leaves_it_out() {
        let blocks = [crate::panel::heading("Left the call")];
        let rows = panel_rows(async |bot: &Bot| {
            bot.panel("c", &blocks, &PanelOptions::default())
                .await
                .unwrap();
            bot.edit_panel(7, &blocks).await.unwrap();
            bot.edit_panel_live(7, &blocks, false).await.unwrap();
        })
        .await;
        assert_eq!(rows[0]["blocks"], json!({ "v": 1, "blocks": blocks }));
        for row in &rows[1..] {
            assert_eq!(row, &json!({ "blocks": { "v": 1, "blocks": blocks } }));
        }
    }

    /// One pass at a time, and each command once: a press counted twice is a
    /// wrong answer, not a duplicate.
    #[tokio::test]
    async fn each_command_is_delivered_once_in_order() {
        let rows = json!([
            { "id": 5, "channel_id": "c", "sender_id": "m", "ciphertext": "/a",
              "action_id": null, "action_value": null, "reply_to": null },
            { "id": 6, "channel_id": "c", "sender_id": "m", "ciphertext": null,
              "action_id": "skip", "action_value": null, "reply_to": 3 },
        ]);
        let server = TestServer::start(move |req| {
            // Only rows past the cursor, as PostgREST would.
            let after: i64 = req
                .path
                .split("id=gt.")
                .nth(1)
                .and_then(|s| s.split('&').next()?.parse().ok())
                .unwrap_or(0);
            let fresh: Vec<Value> = rows
                .as_array()
                .unwrap()
                .iter()
                .filter(|r| r["id"].as_i64().unwrap() > after)
                .cloned()
                .collect();
            (200, Value::Array(fresh))
        })
        .await;
        let bot = Bot::with_options(server.session("bot"), Duration::from_secs(3600), false);
        let got = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (g, c) = (got.clone(), calls.clone());
        *bot.inner.on_message.lock().unwrap() = Some(boxed(move |m: BotMessage| {
            g.lock().unwrap().push(m);
            c.fetch_add(1, Ordering::Relaxed);
            async {}
        }));

        tokio::join!(bot.drain(), bot.drain());
        bot.drain().await;
        let got = got.lock().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(got[0].command(), "a");
        assert!(got[1].is_action());
        assert_eq!(got[1].panel_id, Some(3));
    }

    /// A refusal on its merits is reported, never swallowed by signing in
    /// again; a handler's own error is reported too.
    #[tokio::test]
    async fn a_refusal_is_reported_not_retried_as_a_login() {
        let server = TestServer::start(|req| {
            if req.path.contains("/functions/v1/login") {
                panic!("a 403 must not send the bot round the login path");
            }
            (403, json!({ "message": "permission denied" }))
        })
        .await;
        let bot = Bot::with_options(server.session("bot"), Duration::from_secs(3600), false);
        let errors = Arc::new(Mutex::new(Vec::new()));
        let e = errors.clone();
        bot.on_error(move |error| e.lock().unwrap().push(error));
        bot.drain().await;
        let errors = errors.lock().unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].status, Some(403));
    }
}

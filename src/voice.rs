//! A bot in a voice channel.
//!
//! **A bot publishes; it does not hear.** Two things hold that, and only the
//! second is arithmetic. The token is minted with `canSubscribe: false` unless
//! an admin granted listening. And the call is end-to-end encrypted, so what a
//! bot can decrypt is decided by which key it was given:
//!
//! ```text
//! botKey = HMAC-SHA256(channelKey, "voicebot:v1:<botId>")
//! ```
//!
//! Every member holds `channelKey` and derives `botKey`, so the room hears the
//! bot. The bot is sealed only `botKey`, and HMAC does not run backwards, so it
//! cannot reach `channelKey` (BOTS.md §2, §6b). A member's client seals it; the
//! bot never sees the channel key.
//!
//! The media is LiveKit's Rust SDK, from our fork of it (`Cargo.toml`), which
//! says a two-channel track is stereo. Without that, every bot is mono.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use livekit::e2ee::key_provider::{KeyProvider, KeyProviderOptions};
use livekit::e2ee::manager::E2eeManager;
use livekit::e2ee::{E2eeOptions, EncryptionType};
use livekit::options::{TrackPublishOptions, audio};
use livekit::prelude::*;
use livekit::webrtc::audio_frame::AudioFrame;
use livekit::webrtc::audio_source::native::NativeAudioSource;
use livekit::webrtc::audio_source::{AudioSourceOptions, RtcAudioSource};
use livekit::webrtc::stats::RtcStats;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::watch;

use crate::error::{BotError, Result};
use crate::sealed::{Wrapped, unwrap_key};
use crate::session::BotSession;

/// What `ffmpeg -f s16le -ar 48000 -ac 2` produces, and what is published.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u32 = 2;

/// 10 ms, LiveKit's own tick: larger frames add latency to a skip, smaller
/// ones spend more time in the bridge than in the codec.
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE / 100) as usize;

/// Bytes in one frame: samples × channels × two bytes a sample.
const FRAME_BYTES: usize = FRAME_SAMPLES * CHANNELS as usize * 2;

/// How many key slots LiveKit's ring holds; a member's key goes in
/// `version % KEY_RING_SIZE` (WIRE.md §6).
pub const KEY_RING_SIZE: u32 = 16;

/// The ring slot a bot's media always occupies, whatever its key's version.
/// Members read a bot's key from here (WIRE.md §6, `livekit_e2ee.dart`).
pub const BOT_KEY_INDEX: i32 = 0;

/// `music`: 128 kbps, LiveKit's `MUSIC_HIGH_QUALITY_STEREO`.
pub const MUSIC_BITRATE: u64 = 128_000;

/// How much sound waits in LiveKit's queue ahead of what is playing. Feeding
/// faster than real time fills it and then waits, so it is also the pacing;
/// [`VoiceConnection::stop`] empties it, so it is not the latency of a skip.
const QUEUE_MS: u32 = 500;

/// How long [`join_voice`] waits for a member to seal a media key, and how
/// often it asks.
const MEDIA_KEY_WAIT: Duration = Duration::from_secs(15);
const MEDIA_KEY_POLL: Duration = Duration::from_millis(500);

/// What [`join_voice`] connects as.
#[derive(Debug, Clone, Default)]
pub struct VoiceOptions {
    /// Distinguishes two connections held by the same bot. Identity is
    /// `<userId>~<deviceId>`, and a second connection reusing the first's id
    /// disconnects it. Defaults to the channel id, unique by construction.
    pub device_id: Option<String>,
}

/// How [`VoiceConnection::play`] publishes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Quality {
    /// LiveKit's settings for somebody talking: Opus at WebRTC's default
    /// rate, and DTX, which sends next to nothing while it is quiet.
    #[default]
    Voice,
    /// 128 kbps stereo with DTX and RED off. At the voice settings a song came
    /// through dull and smeared, every quiet passage coded as near-silence.
    /// RED, which sends each packet twice, would double what every listener
    /// downloads at this rate, and Opus has its own loss recovery.
    Music,
}

/// A bot in one call.
pub struct VoiceConnection {
    channel_id: String,
    can_hear: bool,
    room: Room,
    playing: Arc<AtomicBool>,
    source: std::sync::Mutex<Option<NativeAudioSource>>,
    closed: watch::Receiver<bool>,
}

impl VoiceConnection {
    pub fn channel_id(&self) -> &str {
        &self.channel_id
    }

    /// Whether this bot was granted listening here, read from the token's own
    /// grant, which is what LiveKit enforces. A transcription bot that was
    /// never granted would otherwise sit receiving nothing and look like a bug
    /// in its own audio pipeline.
    pub fn can_hear(&self) -> bool {
        self.can_hear
    }

    /// The room, for anything this module does not wrap.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Whether the bot is still in the room.
    pub fn connected(&self) -> bool {
        !*self.closed.borrow()
    }

    /// Resolves once the bot is out of the room for any reason: [`leave`],
    /// or the server taking it out (a member dismissing it, its role losing
    /// the channel, the room closing). The second kind is the one a bot cannot
    /// see coming, and the connection is the only thing that knows. A closed
    /// connection cannot play again: join anew.
    ///
    /// [`leave`]: Self::leave
    pub async fn closed(&self) {
        let mut closed = self.closed.clone();
        let _ = closed.wait_for(|c| *c).await;
    }

    pub fn playing(&self) -> bool {
        self.playing.load(Ordering::SeqCst)
    }

    /// Publish 16-bit little-endian PCM at 48 kHz stereo, interleaved, and
    /// return when it runs out or [`stop`](Self::stop) is called.
    ///
    /// Anything readable works; an `ffmpeg` child's stdout is the usual one,
    /// which turns "a URL" into something publishable without this crate
    /// having an opinion about codecs. Each call publishes a track and takes
    /// it down at the end.
    pub async fn play(&self, pcm: impl AsyncRead + Unpin, quality: Quality) -> Result<()> {
        if !self.connected() {
            return Err(BotError::new("this connection has left its room"));
        }
        if self.playing.swap(true, Ordering::SeqCst) {
            return Err(BotError::new("already playing on this connection"));
        }
        let result = self.feed(pcm, quality).await;
        self.playing.store(false, Ordering::SeqCst);
        result
    }

    async fn feed(&self, mut pcm: impl AsyncRead + Unpin, quality: Quality) -> Result<()> {
        let output = self.publish(quality).await?;
        let mut frames = FrameCutter::default();
        let mut chunk = vec![0u8; 64 * 1024];
        let fed = loop {
            if !self.playing() || !self.connected() {
                break Ok(());
            }
            let n = match pcm.read(&mut chunk).await {
                Ok(0) => break Ok(()),
                Ok(n) => n,
                Err(e) => break Err(BotError::new(format!("reading the sound: {e}"))),
            };
            for samples in frames.push(&chunk[..n]) {
                if !self.playing() {
                    break;
                }
                output.write(&samples).await;
            }
        };
        if self.playing() && self.connected() {
            output.finish().await;
        } else {
            output.close().await;
        }
        fed
    }

    /// Publish a track to write sound into yourself, for a bot that keeps one
    /// track for a whole visit and decides frame by frame what goes in it: a
    /// queue, a pause, a volume. [`play`](Self::play) is this with a stream.
    ///
    /// Publishing is what members see as the bot starting to speak, so one
    /// track per visit rather than one per song keeps the bot from dropping
    /// out of the call between songs.
    pub async fn publish(&self, quality: Quality) -> Result<AudioOutput<'_>> {
        if !self.connected() {
            return Err(BotError::new("this connection has left its room"));
        }
        let source = NativeAudioSource::new(AudioSourceOptions::default(), SAMPLE_RATE, CHANNELS, QUEUE_MS);
        let track = LocalAudioTrack::create_audio_track("audio", RtcAudioSource::Native(source.clone()));
        let mut options = TrackPublishOptions { source: TrackSource::Microphone, ..Default::default() };
        if quality == Quality::Music {
            options.audio_encoding = Some(audio::MUSIC_HIGH_QUALITY_STEREO.encoding);
            options.dtx = false;
            options.red = false;
        }
        let publication = self
            .room
            .local_participant()
            .publish_track(LocalTrack::Audio(track), options)
            .await
            .map_err(|e| BotError::new(format!("could not publish: {e}")))?;
        *self.source.lock().unwrap() = Some(source.clone());
        Ok(AudioOutput { voice: self, source, sid: publication.sid() })
    }

    /// Stop the current [`play`](Self::play) now, queued sound included. The
    /// stream is left to whoever owns it. An [`AudioOutput`]'s queue is
    /// emptied too.
    pub fn stop(&self) {
        self.playing.store(false, Ordering::SeqCst);
        if let Some(source) = self.source.lock().unwrap().as_ref() {
            source.clear_buffer();
        }
    }

    /// WebRTC's own numbers for what is being sent: bitrate, codec and its
    /// parameters, packets lost. Where to look when the sound is wrong.
    pub async fn stats(&self) -> Vec<RtcStats> {
        let mut out = Vec::new();
        for publication in self.room.local_participant().track_publications().values() {
            if let Some(track) = publication.track() {
                out.extend(track.get_stats().await.unwrap_or_default());
            }
        }
        out
    }

    /// The Opus parameters each published track was sent with, as WebRTC
    /// reports them: `stereo=1` is in there when the call is stereo.
    pub async fn opus_fmtp(&self) -> Vec<String> {
        self.stats()
            .await
            .into_iter()
            .filter_map(|s| match s {
                RtcStats::Codec(c) if c.codec.mime_type.contains("opus") => Some(c.codec.sdp_fmtp_line),
                _ => None,
            })
            .collect()
    }

    /// Leave the call.
    pub async fn leave(&self) -> Result<()> {
        self.stop();
        self.room.close().await.map_err(|e| BotError::new(format!("leaving: {e}")))
    }
}

/// A published track a bot writes sound into, from
/// [`VoiceConnection::publish`]. Take it down with [`finish`](Self::finish)
/// or [`close`](Self::close); dropped, it stays published until the bot
/// leaves.
pub struct AudioOutput<'a> {
    voice: &'a VoiceConnection,
    source: NativeAudioSource,
    sid: TrackSid,
}

impl AudioOutput<'_> {
    /// Queue interleaved 48 kHz stereo samples. Waits while LiveKit's queue is
    /// full, which is what paces a writer that has the whole song at once.
    /// A frame LiveKit refuses is logged and dropped: one lost 10 ms is a
    /// click, and stopping the song for it would be worse.
    pub async fn write(&self, samples: &[i16]) {
        let frame = AudioFrame {
            data: samples.into(),
            sample_rate: SAMPLE_RATE,
            num_channels: CHANNELS,
            samples_per_channel: (samples.len() / CHANNELS as usize) as u32,
        };
        if let Err(e) = self.source.capture_frame(&frame).await {
            log::warn!("voice: LiveKit refused a frame: {e}");
        }
    }

    /// Drop what is queued and not yet sent, so a skip or a pause is heard at
    /// once rather than half a second later.
    pub fn clear(&self) {
        self.source.clear_buffer();
    }

    /// Let what is queued play out, then take the track down.
    pub async fn finish(self) {
        tokio::time::sleep(Duration::from_millis(QUEUE_MS as u64)).await;
        self.close().await;
    }

    /// Take the track down now.
    pub async fn close(self) {
        self.voice.source.lock().unwrap().take();
        if self.voice.connected() {
            let _ = self.voice.room.local_participant().unpublish_track(&self.sid).await;
        }
    }
}

/// Connect to a channel's room.
///
/// The token comes from `get_channel_token`, the edge function the app calls:
/// there is no bot path, which is what stops the two drifting.
pub async fn join_voice(session: &BotSession, channel_id: &str, options: &VoiceOptions) -> Result<VoiceConnection> {
    // The media key first: without it the bot would connect, publish frames
    // nobody can decrypt, and look like a working bot everyone has muted.
    let key = media_key(session, channel_id).await?;

    let data = session
        .call_function(
            "get_channel_token",
            &json!({
                "channel_id": channel_id,
                "device_id": options.device_id.as_deref().unwrap_or(channel_id),
            }),
        )
        .await?;
    let token = data["token"].as_str().ok_or_else(|| BotError::new("get_channel_token returned no token"))?;
    // A server with more than one region holds each call on one node, and the
    // token is signed for that node alone. The reply says which; the default
    // is for a server too old to.
    let url = match data["livekit_url"].as_str() {
        Some(url) if !url.is_empty() => url.to_string(),
        _ => livekit_url(session).await?,
    };
    connect(channel_id, &url, token, &key).await
}

/// Join with a token and the bot's media key, encrypting in slot 0.
pub(crate) async fn connect(channel_id: &str, url: &str, token: &str, key: &[u8]) -> Result<VoiceConnection> {
    // Shared-key mode is right for a bot and only a bot: it publishes with one
    // key and subscribes to nothing it could decrypt anyway. Members run per
    // participant, which is what lets them hold a different key for the bot.
    let key_provider = KeyProvider::with_shared_key(KeyProviderOptions::default(), key.to_vec());
    key_provider.set_shared_key(key.to_vec(), BOT_KEY_INDEX);
    let mut room_options = RoomOptions::default();
    room_options.encryption = Some(E2eeOptions { encryption_type: EncryptionType::Gcm, key_provider });

    let (room, events) = Room::connect(url, token, room_options)
        .await
        .map_err(|e| BotError::new(format!("could not join the call: {e}")))?;
    let (closed_tx, closed) = watch::channel(false);
    let playing = Arc::new(AtomicBool::new(false));
    watch_room(room.e2ee_manager().clone(), events, closed_tx, playing.clone());
    Ok(VoiceConnection {
        channel_id: channel_id.to_string(),
        can_hear: grant_allows_subscribe(token),
        room,
        playing,
        source: std::sync::Mutex::new(None),
        closed,
    })
}

/// Follow the room for as long as it lasts: settle `closed` when it ends, and
/// keep every cryptor on [`BOT_KEY_INDEX`].
///
/// The SDK makes a sender's cryptor on slot 0 already, so the pinning says
/// out loud what is otherwise a default, and survives a reconnect, which
/// republishes every track with a fresh cryptor.
fn watch_room(
    e2ee: E2eeManager,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RoomEvent>,
    closed: watch::Sender<bool>,
    playing: Arc<AtomicBool>,
) {
    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            match event {
                RoomEvent::LocalTrackPublished { .. } | RoomEvent::Reconnected => {
                    for (_, cryptor) in e2ee.frame_cryptors() {
                        cryptor.set_key_index(BOT_KEY_INDEX);
                    }
                }
                RoomEvent::Disconnected { .. } => break,
                _ => {}
            }
        }
        // Ends a running play at its next frame, rather than leaving it to
        // fail on a room that is gone.
        playing.store(false, Ordering::SeqCst);
        closed.send_replace(true);
    });
}

#[derive(Deserialize)]
struct SealedVoiceKey {
    #[serde(flatten)]
    wrapped: Wrapped,
}

/// The key this bot's media is encrypted with in `channel_id`.
///
/// **Waited for, not merely asked for.** The summon is what causes the
/// sealing: a member's client sees it and seals in answer. So the first `/play`
/// into a channel is a race a bot that asked once always lost (0.7 s measured
/// between the summon and the key). It still gives up, because waiting cannot
/// help when nobody is there to seal.
async fn media_key(session: &BotSession, channel_id: &str) -> Result<Vec<u8>> {
    let deadline = Instant::now() + MEDIA_KEY_WAIT;
    loop {
        let data = session.call_function("get_channel_key", &json!({ "channel_id": channel_id })).await?;
        if let Ok(sealed) = serde_json::from_value::<SealedVoiceKey>(data["my_voice_key"].clone()) {
            return unwrap_key(session.chat_identity(), &sealed.wrapped)
                .ok_or_else(|| BotError::new("the media key sealed for this bot does not open"));
        }
        if Instant::now() >= deadline {
            return Err(BotError::new(
                "No media key for this channel after waiting. A member has to be there to seal \
                 one: their client does it, in answer to the summon, and nobody answered.",
            ));
        }
        tokio::time::sleep(MEDIA_KEY_POLL).await;
    }
}

/// Where this server's LiveKit lives, for a server whose token reply does not
/// say. On the server's own row, which every member can read.
async fn livekit_url(session: &BotSession) -> Result<String> {
    #[derive(Deserialize)]
    struct Row {
        livekit_url: Option<String>,
    }
    let rows: Vec<Row> = session
        .select(&format!("servers?select=livekit_url&id=eq.{}", session.server_id()))
        .await?;
    rows.into_iter()
        .next()
        .and_then(|r| r.livekit_url)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| BotError::new("this server has no LiveKit URL configured"))
}

/// `video.canSubscribe` out of the LiveKit JWT, which defaults to true.
/// Unreadable is false: a bot told it can hear when it cannot spends its time
/// debugging its audio pipeline.
pub fn grant_allows_subscribe(token: &str) -> bool {
    let claims = token
        .split('.')
        .nth(1)
        .and_then(|p| URL_SAFE_NO_PAD.decode(p.trim_end_matches('=')).ok())
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match claims {
        Some(claims) => claims["video"]["canSubscribe"] != Value::Bool(false),
        None => false,
    }
}

/// Cuts a byte stream into whole 10 ms frames.
///
/// A stream hands over whatever the pipe had, so a chunk boundary almost never
/// lands on a frame boundary. Publishing a short frame at each one instead of
/// carrying the tail forward puts an audible click wherever chunks fall,
/// several a second on a 64 KB pipe: a bug you can only hear, so it is tested.
/// A trailing partial frame at the end is dropped, not padded.
#[derive(Default)]
pub struct FrameCutter {
    carry: Vec<u8>,
}

impl FrameCutter {
    pub fn push(&mut self, chunk: &[u8]) -> Vec<Vec<i16>> {
        self.carry.extend_from_slice(chunk);
        let whole = self.carry.len() / FRAME_BYTES * FRAME_BYTES;
        let frames = self.carry[..whole]
            .chunks_exact(FRAME_BYTES)
            .map(|frame| frame.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]])).collect())
            .collect();
        self.carry.drain(..whole);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(samples: impl IntoIterator<Item = i16>) -> Vec<u8> {
        samples.into_iter().flat_map(i16::to_le_bytes).collect()
    }

    #[test]
    fn a_frame_is_10ms_of_48khz_stereo() {
        assert_eq!(FRAME_SAMPLES, 480);
        assert_eq!(FRAME_BYTES, 1920);
    }

    #[test]
    fn a_chunk_boundary_mid_frame_carries_into_the_next_chunk() {
        let bytes = pcm((0..960 * 2).map(|i| i as i16));
        let mut cutter = FrameCutter::default();
        let first = cutter.push(&bytes[..1000]);
        let second = cutter.push(&bytes[1000..]);
        assert!(first.is_empty());
        assert_eq!(second.len(), 2);
        // The samples either side of the boundary are the ones that were sent.
        assert_eq!(second[0][499], 499);
        assert_eq!(second[0][500], 500);
        assert_eq!(second[1][0], 960);
    }

    #[test]
    fn small_chunks_accumulate_and_an_odd_byte_carries() {
        let bytes = pcm((0..960).map(|i| i as i16));
        let mut cutter = FrameCutter::default();
        let mut frames = Vec::new();
        for chunk in bytes.chunks(7) {
            frames.extend(cutter.push(chunk));
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], (0..960).map(|i| i as i16).collect::<Vec<_>>());
    }

    #[test]
    fn a_trailing_partial_frame_is_dropped_not_padded() {
        let mut cutter = FrameCutter::default();
        assert_eq!(cutter.push(&vec![0u8; FRAME_BYTES + 10]).len(), 1);
    }

    fn token(claims: Value) -> String {
        format!("h.{}.s", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn what_the_token_says_about_hearing() {
        assert!(!grant_allows_subscribe(&token(json!({ "video": { "canSubscribe": false } }))));
        assert!(grant_allows_subscribe(&token(json!({ "video": { "canSubscribe": true } }))));
        // An absent flag means yes, which is what LiveKit does.
        assert!(grant_allows_subscribe(&token(json!({ "video": {} }))));
        assert!(!grant_allows_subscribe("not a token"));
    }
}

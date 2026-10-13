//! A bot's sound, played into a real room and heard by a real listener.
//!
//! Ignored by default: it needs a LiveKit server. Run it with the server's
//! credentials in the environment:
//!
//! ```text
//! LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=… LIVEKIT_API_SECRET=… \
//!   cargo test --features voice live_ -- --ignored --nocapture
//! ```
//!
//! What it proves: the bot's two channels reach a listener as two, through
//! the call's encryption, in the slot members read a bot's key from. A tone
//! fed into only the left ear has to come out of only the left ear. Without
//! our fork of LiveKit (`Cargo.toml`) it comes out of both.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use livekit::e2ee::key_provider::{KeyProvider, KeyProviderOptions};
use livekit::e2ee::{E2eeOptions, EncryptionType};
use livekit::prelude::*;
use livekit::webrtc::audio_stream::native::NativeAudioStream;
use livekit_api::access_token::{AccessToken, VideoGrants};
use tokio::time::timeout;

use crate::voice::{BOT_KEY_INDEX, CHANNELS, Quality, SAMPLE_RATE, connect};

/// Each ear has the tone alone for this long, in turn.
const SIDE_MS: usize = 1_000;
const PLAY_MS: usize = 8_000;
const LISTEN: Duration = Duration::from_secs(6);
/// The quiet ear must sit this far under the loud one: 20 dB. A mono stream
/// puts the same level in both.
const SEPARATION: f64 = 10.0;

struct Server {
    url: String,
    key: String,
    secret: String,
}

impl Server {
    fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set; see the module doc"));
        Self { url: var("LIVEKIT_URL"), key: var("LIVEKIT_API_KEY"), secret: var("LIVEKIT_API_SECRET") }
    }

    /// A bot's token publishes and does not subscribe, as `get_channel_token`
    /// mints one for a bot with no listening grant.
    fn token(&self, room: &str, identity: &str, subscribe: bool) -> String {
        AccessToken::with_api_key(&self.key, &self.secret)
            .with_identity(identity)
            .with_name(identity)
            .with_grants(VideoGrants {
                room_join: true,
                room: room.to_string(),
                can_publish: true,
                can_subscribe: subscribe,
                ..Default::default()
            })
            .to_jwt()
            .expect("token")
    }
}

fn media_key() -> Vec<u8> {
    (100..132).collect()
}

/// Interleaved 16-bit stereo: a 1 kHz tone in the left ear for a second, then
/// in the right, and so on.
fn tones() -> Vec<u8> {
    let rate = SAMPLE_RATE as usize;
    let mut out = Vec::with_capacity(rate * PLAY_MS / 1000 * 4);
    for i in 0..rate * PLAY_MS / 1000 {
        let left = (i * 1000 / rate / SIDE_MS).is_multiple_of(2);
        let v = ((i as f64 / rate as f64 * 1_000.0 * std::f64::consts::TAU).sin() * 8_000.0) as i16;
        let (l, r) = if left { (v, 0) } else { (0, v) };
        out.extend(l.to_le_bytes());
        out.extend(r.to_le_bytes());
    }
    out
}

fn levels(data: &[i16]) -> (f64, f64) {
    let (mut l, mut r) = (0.0, 0.0);
    for pair in data.chunks_exact(2) {
        l += (pair[0] as f64).powi(2);
        r += (pair[1] as f64).powi(2);
    }
    let n = (data.len() / 2).max(1) as f64;
    ((l / n).sqrt(), (r / n).sqrt())
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a LiveKit server"]
async fn live_a_bot_plays_into_a_call_in_stereo() {
    let server = Server::from_env();
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
    let room = format!("rift-bot-test-{now}");

    let bot = connect("channel", &server.url, &server.token(&room, "bot", false), &media_key())
        .await
        .expect("the bot joins");
    assert!(!bot.can_hear(), "a bot's token here does not subscribe");

    // A listener holding the bot's key in slot 0, where members read it.
    let key_provider = KeyProvider::with_shared_key(KeyProviderOptions::default(), media_key());
    key_provider.set_shared_key(media_key(), BOT_KEY_INDEX);
    let mut options = RoomOptions::default();
    options.encryption = Some(E2eeOptions { encryption_type: EncryptionType::Gcm, key_provider });
    let (listener, mut events) = Room::connect(&server.url, &server.token(&room, "listener", true), options)
        .await
        .expect("the listener joins");

    let pcm = tones();
    let bot = std::sync::Arc::new(bot);
    let player = bot.clone();
    let playing = tokio::spawn(async move { player.play(pcm.as_slice(), Quality::Music).await });

    let track = timeout(Duration::from_secs(20), async {
        loop {
            if let RoomEvent::TrackSubscribed { track: RemoteTrack::Audio(track), .. } =
                events.recv().await.expect("room events")
            {
                break track;
            }
        }
    })
    .await
    .expect("the listener gets the sound");

    let mut stream = NativeAudioStream::new(track.rtc_track(), SAMPLE_RATE as i32, CHANNELS as i32);
    let (mut left_only, mut right_only, mut loud) = (0, 0, 0);
    let _ = timeout(LISTEN, async {
        while let Some(frame) = stream.next().await {
            let (l, r) = levels(&frame.data);
            if l.max(r) < 500.0 {
                continue;
            }
            loud += 1;
            if l > r * SEPARATION {
                left_only += 1;
            } else if r > l * SEPARATION {
                right_only += 1;
            }
        }
    })
    .await;
    let fmtp = bot.opus_fmtp().await;
    println!("sent with {fmtp:?}; {loud} loud frames, {left_only} left only, {right_only} right only");

    bot.stop();
    playing.await.unwrap().expect("play ends cleanly when stopped");
    bot.leave().await.ok();
    listener.close().await.ok();

    assert!(loud > 100, "the listener heard {loud} frames of the tone");
    assert!(
        left_only > loud / 4 && right_only > loud / 4,
        "each ear should have the tone alone a good part of the time: \
         {left_only} left only, {right_only} right only, of {loud}"
    );
}

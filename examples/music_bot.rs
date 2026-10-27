//! A music bot: `/play <url>` joins the voice channel you are in and plays it,
//! in stereo, with a panel to stop it.
//!
//!     cargo run --features voice --example music_bot -- <config file> <seed file>
//!
//! Needs `ffmpeg` on PATH. This crate has no opinion about codecs: it
//! publishes 16-bit PCM, and turning "a URL" into that is what ffmpeg is for.
//!
//! The bot hears nothing while it plays, and needs no grant to do it.
//! Publishing is the half that was never the problem; a bot that could listen
//! to a room full of people is the half an admin has to allow.

mod common;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use rift_bot::panel::{actions, button, heading, text};
use rift_bot::voice::{CHANNELS, Quality, SAMPLE_RATE, VoiceConnection, VoiceOptions};
use rift_bot::{Bot, BotError, BotMessage, PanelOptions};
use serde_json::json;
use tokio::process::{Child, Command};

/// The call outlives any one track: `/stop` ends what is playing and stays,
/// `/disconnect` is what leaves.
struct Call {
    voice: Arc<VoiceConnection>,
    /// What is playing: the decoder, and the panel that shows it.
    track: Option<(Child, Option<i64>, String)>,
}

type Calls = Arc<Mutex<HashMap<String, Call>>>;

/// Decode anything ffmpeg understands into what LiveKit wants.
fn decode(source: &str) -> std::io::Result<Child> {
    Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-i",
            source,
            "-f",
            "s16le",
        ])
        .args([
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-ac",
            &CHANNELS.to_string(),
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
}

/// Where to play: the summon first, the roster second.
///
/// A summon is written by the caller's own client when they send a command
/// this bot's manifest marks `summon: true`, and it is the only source that
/// works for a **private** voice channel, which a bot cannot otherwise see.
async fn voice_channel_of(bot: &Bot, user_id: &str) -> Result<Option<String>, BotError> {
    if let Some((channel, _)) = bot
        .summons()
        .await?
        .into_iter()
        .find(|(_, by)| by.as_deref() == Some(user_id))
    {
        return Ok(Some(channel));
    }
    let data = bot
        .session()
        .call_function("voice_roster", &json!({}))
        .await?;
    Ok(data["roster"][user_id].as_str().map(str::to_string))
}

/// End the track and **stay in the call**: somebody who wanted quiet for a
/// minute should not have to summon the bot back.
async fn stop_playing(bot: &Bot, calls: &Calls, channel_id: &str) -> Result<(), BotError> {
    let track = {
        let mut calls = calls.lock().unwrap();
        let Some(call) = calls.get_mut(channel_id) else {
            return Ok(());
        };
        call.voice.stop();
        call.track.take()
    };
    if let Some((_decoder, Some(panel_id), _)) = track {
        bot.edit_panel(panel_id, &[heading("Stopped")]).await?;
    }
    Ok(())
}

/// End the track and leave. Dismissing itself drops the summon and the media
/// key with it, so members stop seeing the bot listed in the call.
async fn disconnect(bot: &Bot, calls: &Calls, channel_id: &str) -> Result<(), BotError> {
    stop_playing(bot, calls, channel_id).await?;
    let call = calls.lock().unwrap().remove(channel_id);
    if let Some(call) = call {
        call.voice.leave().await?;
    }
    bot.dismiss_self(channel_id).await
}

async fn play(
    bot: &Bot,
    calls: &Calls,
    message: &BotMessage,
    source: &str,
) -> Result<(), BotError> {
    let Some(channel) = voice_channel_of(bot, &message.sender_id).await? else {
        return bot
            .reply_privately(message, "Join a voice channel first.")
            .await;
    };
    stop_playing(bot, calls, &channel).await?;

    // Joined once and kept: leaving between tracks would mean a fresh token, a
    // fresh key and a participant that flickers in and out of the room.
    let existing = calls
        .lock()
        .unwrap()
        .get(&channel)
        .map(|c| c.voice.clone())
        .filter(|v| v.connected());
    let voice = match existing {
        Some(voice) => voice,
        None => {
            let voice = Arc::new(bot.join_voice(&channel, &VoiceOptions::default()).await?);
            calls.lock().unwrap().insert(
                channel.clone(),
                Call {
                    voice: voice.clone(),
                    track: None,
                },
            );
            // The room can end without the bot hearing why (a member's client
            // dismissing it in a private channel), so the connection says so.
            let (bot, calls, channel, watched) =
                (bot.clone(), calls.clone(), channel.clone(), voice.clone());
            tokio::spawn(async move {
                watched.closed().await;
                let ours = calls
                    .lock()
                    .unwrap()
                    .get(&channel)
                    .is_some_and(|c| Arc::ptr_eq(&c.voice, &watched));
                if ours {
                    let _ = stop_playing(&bot, &calls, &channel).await;
                    calls.lock().unwrap().remove(&channel);
                }
            });
            voice
        }
    };

    let mut decoder = decode(source).map_err(|e| BotError::new(format!("starting ffmpeg: {e}")))?;
    let pcm = decoder
        .stdout
        .take()
        .ok_or_else(|| BotError::new("ffmpeg has no stdout"))?;
    let panel_id = bot
        .panel(
            &message.channel_id,
            &[
                heading("Now playing"),
                text(source),
                actions(&[button("Stop", "stop").danger()]),
            ],
            &PanelOptions::default(),
        )
        .await?;
    if let Some(call) = calls.lock().unwrap().get_mut(&channel) {
        call.track = Some((decoder, panel_id, source.to_string()));
    }

    // Not awaited here: the handler has to return so the next command is
    // answered while this track is still going.
    let (bot, calls) = (bot.clone(), calls.clone());
    tokio::spawn(async move {
        if let Err(error) = voice.play(pcm, Quality::Music).await {
            eprintln!("playback failed: {error}");
        }
        // Tidy up only if this is still the track playing: a `/play` that
        // replaced it has already started its own.
        let still_ours = calls
            .lock()
            .unwrap()
            .get(&channel)
            .and_then(|c| c.track.as_ref())
            .is_some_and(|t| t.1 == panel_id);
        if still_ours {
            let _ = stop_playing(&bot, &calls, &channel).await;
        }
    });
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (session, _) = common::session_from_args()?;
    let bot = Bot::new(session);
    let calls: Calls = Arc::default();

    let (handle, state) = (bot.clone(), calls.clone());
    bot.listen(move |message| {
        let (bot, calls) = (handle.clone(), state.clone());
        async move {
            if message.is_action() {
                if message.action_id.as_deref() != Some("stop") {
                    return Ok(());
                }
                let channel = calls.lock().unwrap().iter().find_map(|(id, call)| {
                    call.track
                        .as_ref()
                        .filter(|t| t.1.is_some() && t.1 == message.panel_id)
                        .map(|_| id.clone())
                });
                return match channel {
                    Some(channel) => stop_playing(&bot, &calls, &channel).await,
                    None => Ok(()),
                };
            }
            match message.command().as_str() {
                "play" if message.args().is_empty() => {
                    bot.reply_privately(&message, "Usage: /play <url>").await
                }
                "play" => play(&bot, &calls, &message, message.args()).await,
                "stop" | "disconnect" => {
                    let Some(channel) = voice_channel_of(&bot, &message.sender_id).await? else {
                        return Ok(());
                    };
                    if message.command() == "stop" {
                        stop_playing(&bot, &calls, &channel).await
                    } else {
                        disconnect(&bot, &calls, &channel).await
                    }
                }
                _ => Ok(()),
            }
        }
    })
    .await?;

    // Rift knows none of these verbs. `summon` and `dismiss` say what a command
    // means, and the member's client acts on them: `dismiss` on `/disconnect`
    // drops the summon even if this bot has crashed.
    bot.session()
        .publish_manifest(&json!({
            "description": "Plays a URL in your voice channel.",
            "commands": [
                { "name": "play", "description": "Play something", "usage": "<url>", "summon": true },
                { "name": "stop", "description": "Stop the track, stay in the call" },
                { "name": "disconnect", "description": "Stop and leave the call", "dismiss": true },
            ],
        }))
        .await?;

    println!("playing. ctrl-c to stop.");
    tokio::signal::ctrl_c().await?;
    bot.stop();
    Ok(())
}

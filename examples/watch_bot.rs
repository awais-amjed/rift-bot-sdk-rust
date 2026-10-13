//! A bot that reads a channel it was granted: the one exception in BOTS.md §6.
//!
//!     cargo run --example watch_bot -- <config file> <seed file> <channel id>
//!
//! Everything else a bot does, it does because somebody addressed it. This is
//! the one place it holds the channel key and reads a conversation nobody
//! pointed at it, because a moderation bot cannot work any other way. It only
//! works after an admin grants it, per channel, and the channel says so.

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rift_bot::Bot;

const WINDOW: Duration = Duration::from_secs(10);
const LIMIT: usize = 5;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (session, rest) = common::session_from_args()?;
    let channel_id = rest.first().ok_or("usage: watch_bot <config file> <seed file> <channel id>")?.clone();
    let bot = Bot::new(session);

    // A toy rule, and the shape a real one has: count what arrives, say
    // something when a threshold trips.
    let recent: Arc<Mutex<HashMap<String, Vec<Instant>>>> = Arc::default();
    let (poster, channel) = (bot.clone(), channel_id.clone());
    let granted = bot
        .watch_channel(&channel_id, move |message| {
            let (bot, recent, channel) = (poster.clone(), recent.clone(), channel.clone());
            async move {
                println!("[{}] {}: {}", message.sent_at, message.sender_id, message.text);
                let count = {
                    let mut recent = recent.lock().unwrap();
                    let times = recent.entry(message.sender_id).or_default();
                    times.retain(|t| t.elapsed() < WINDOW);
                    times.push(Instant::now());
                    times.len()
                };
                if count == LIMIT {
                    // In the clear, even in a channel it can read: a member
                    // can always tell which half of the room a line came from.
                    bot.post(&channel, &format!("That is {LIMIT} messages in ten seconds.")).await?;
                }
                Ok::<(), rift_bot::BotError>(())
            }
        })
        .await?;

    let Some(from_version) = granted else {
        eprintln!("Not granted {channel_id}. An admin has to hand this bot the channel key first.");
        std::process::exit(1);
    };
    println!("Reading {channel_id} from key version {from_version}.");
    bot.listen(|_| async {}).await?;
    tokio::signal::ctrl_c().await?;
    bot.stop();
    Ok(())
}

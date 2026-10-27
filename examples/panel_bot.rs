//! A bot with a panel: `/poll <question>` posts one, and pressing a button
//! redraws it in place rather than posting a new line. A poll that announced
//! every vote as a message would be the log a panel exists to replace.
//!
//!     cargo run --example panel_bot -- <config file> <seed file>

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rift_bot::panel::{Block, actions, button, fields, heading, progress};
use rift_bot::{Bot, PanelOptions};
use serde_json::json;

struct Poll {
    question: String,
    yes: u32,
    no: u32,
}

fn draw(poll: &Poll) -> Vec<Block> {
    let total = poll.yes + poll.no;
    vec![
        heading(&poll.question),
        fields(&[("Yes", &poll.yes.to_string()), ("No", &poll.no.to_string())]),
        progress(
            if total == 0 {
                0.0
            } else {
                poll.yes as f64 / total as f64
            },
            &if total == 1 {
                "1 vote".to_string()
            } else {
                format!("{total} votes")
            },
        ),
        actions(&[button("Yes", "yes").primary(), button("No", "no")]),
    ]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (session, _) = common::session_from_args()?;
    session.login().await?;
    session
        .publish_manifest(&json!({
            "description": "Runs a poll on a panel",
            "commands": [{ "name": "poll", "description": "Start a poll", "usage": "/poll <question>" }],
        }))
        .await?;

    // Panel id → its tallies. In memory, so a restart forgets: this is an
    // example of the shape, not of how to keep state.
    let polls: Arc<Mutex<HashMap<i64, Poll>>> = Arc::default();
    let bot = Bot::new(session);
    let handle = bot.clone();
    bot.listen(move |message| {
        let (bot, polls) = (handle.clone(), polls.clone());
        async move {
            // A press, not a command: `panel_id` is the row to redraw.
            if message.is_action() {
                let Some(id) = message.panel_id else {
                    return Ok(());
                };
                let blocks = {
                    let mut polls = polls.lock().unwrap();
                    let Some(poll) = polls.get_mut(&id) else {
                        return Ok(());
                    };
                    if message.action_id.as_deref() == Some("yes") {
                        poll.yes += 1
                    } else {
                        poll.no += 1
                    }
                    draw(poll)
                };
                return bot.edit_panel(id, &blocks).await;
            }
            if message.command() != "poll" {
                return Ok(());
            }
            let question = match message.args() {
                "" => "Yes or no?".to_string(),
                text => text.to_string(),
            };
            let poll = Poll {
                question,
                yes: 0,
                no: 0,
            };
            if let Some(id) = bot
                .panel(&message.channel_id, &draw(&poll), &PanelOptions::default())
                .await?
            {
                polls.lock().unwrap().insert(id, poll);
            }
            Ok::<(), rift_bot::BotError>(())
        }
    })
    .await?;

    println!("panel bot listening. ctrl-c to stop.");
    tokio::signal::ctrl_c().await?;
    bot.stop();
    Ok(())
}

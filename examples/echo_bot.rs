//! A bot in about thirty lines.
//!
//!     cargo run --example echo_bot -- <config file> <seed file>

mod common;

use rift_bot::Bot;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (session, _) = common::session_from_args()?;
    session.login().await?;
    session
        .publish_manifest(&json!({
            "description": "Says what you said",
            "commands": [{ "name": "echo", "description": "Repeat something", "usage": "/echo <text>" }],
        }))
        .await?;

    let bot = Bot::new(session);
    let replier = bot.clone();
    bot.listen(move |message| {
        let bot = replier.clone();
        async move {
            if message.command() != "echo" {
                return Ok(());
            }
            match message.args() {
                "" => bot.reply_privately(&message, "Give me something to echo.").await,
                text => bot.reply(&message, text).await,
            }
        }
    })
    .await?;

    println!("echo bot listening. ctrl-c to stop.");
    tokio::signal::ctrl_c().await?;
    bot.stop();
    Ok(())
}

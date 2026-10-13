//! A bot that answers its DMs.
//!
//!     cargo run --example dm_bot -- <config file> <seed file>
//!
//! The only place a bot does real chat crypto. A DM is sealed to the two of
//! you, and the server cannot read it any more than the other members can
//! (BOTS.md §3). The key is never stored or sent: both ends derive it from
//! opposite halves of an X25519 exchange.

mod common;

use rift_bot::Bot;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let (session, _) = common::session_from_args()?;
    let bot = Bot::new(session);

    let replier = bot.clone();
    bot.on_direct_message(move |dm| {
        let bot = replier.clone();
        // `dm.text` is opened and its signature checked already: a row that
        // failed either never reaches here.
        async move {
            let answer = if dm.text.trim().eq_ignore_ascii_case("ping") {
                "pong".to_string()
            } else {
                format!("You said: {}", dm.text)
            };
            bot.dms().reply(&dm, &answer).await
        }
    });
    // Commands are still heard from the same pass; this one has none.
    bot.listen(|_| async {}).await?;

    println!("answering DMs. ctrl-c to stop.");
    tokio::signal::ctrl_c().await?;
    bot.stop();
    Ok(())
}

//! Adding a bot to a server: one paste.
//!
//!     cargo run --example join -- '<invite link>' <username> <seed file> <config file>
//!
//! The invite is the same link an admin would send a person, in any of its
//! three shapes: an invite already carries exactly the right thing, a
//! per-server grant that whoever minted it can revoke (BOTS.md §1).
//!
//! Run once. An invite is spent, so this saves the session's config beside
//! the seed, and every other example starts from those two files.

use rift_bot::{BotSession, JoinOptions, create_seed, load_seed};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let [invite, username, seed_file, config_file] = args()?;

    // Reused if one is already there, so a re-run cannot strand the old bot.
    let seed = match load_seed(&seed_file) {
        Ok(seed) => seed,
        Err(_) => {
            println!("writing a new seed to {seed_file}: back it up, it cannot be recovered");
            create_seed(&seed_file)?
        }
    };

    let session = BotSession::join(JoinOptions {
        invite: &invite,
        seed: &seed,
        username: &username,
        display_name: None,
    })
    .await?;

    std::fs::write(
        &config_file,
        serde_json::to_string_pretty(&session.config())?,
    )?;
    println!(
        "joined as {username} ({}); saved {config_file}",
        session.user_id().unwrap_or_default()
    );
    Ok(())
}

fn args() -> Result<[String; 4], String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    args.try_into()
        .map_err(|_| "usage: join '<invite link>' <username> <seed file> <config file>".into())
}

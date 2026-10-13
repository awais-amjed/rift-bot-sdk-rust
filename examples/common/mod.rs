//! What every example starts with: a session from the two files `join` wrote.

use rift_bot::{BotSession, SessionConfig, load_seed};

/// `<config file> <seed file>` from the command line, and anything after them.
pub fn session_from_args() -> Result<(BotSession, Vec<String>), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(config_file), Some(seed_file)) = (args.next(), args.next()) else {
        return Err("usage: <example> <config file> <seed file> [more]".into());
    };
    let config: SessionConfig = serde_json::from_str(&std::fs::read_to_string(config_file)?)?;
    let session = BotSession::from_config(&config, &load_seed(seed_file)?)?;
    Ok((session, args.collect()))
}

//! A bot's seed: 32 random bytes that **are** the bot. Whoever holds them can
//! act as it, so keep the file out of version control.

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use crate::crypto::random_bytes;
use crate::error::{BotError, Result};

/// Read a seed file: the 32 bytes in base64, as the TypeScript SDK writes it.
pub fn load_seed(path: impl AsRef<Path>) -> Result<[u8; 32]> {
    let path = path.as_ref();
    let text = std::fs::read_to_string(path)
        .map_err(|e| BotError::new(format!("reading {}: {e}", path.display())))?;
    let bytes = STANDARD
        .decode(text.trim())
        .map_err(|_| BotError::new(format!("{} is not base64", path.display())))?;
    <[u8; 32]>::try_from(bytes.as_slice())
        .map_err(|_| BotError::new(format!("{} does not hold 32 bytes", path.display())))
}

/// Make a new seed and write it to `path`, readable by its owner only.
/// Refuses to replace one: overwriting a seed strands the bot it was.
pub fn create_seed(path: impl AsRef<Path>) -> Result<[u8; 32]> {
    let path = path.as_ref();
    let seed = random_bytes::<32>();
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|e| BotError::new(format!("creating {}: {e}", path.display())))?;
    std::io::Write::write_all(&mut file, STANDARD.encode(seed).as_bytes())
        .map_err(|e| BotError::new(format!("writing {}: {e}", path.display())))?;
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_round_trips_and_is_never_overwritten() {
        let dir = std::env::temp_dir().join(format!("rift-seed-{}", crate::crypto::b64(&random_bytes::<6>()).replace('/', "_")));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bot.seed");
        let seed = create_seed(&path).unwrap();
        assert_eq!(load_seed(&path).unwrap(), seed);
        assert!(create_seed(&path).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

//! Write a bot for [Rift](https://joinrift.app) in Rust.
//!
//! The formats this crate must match byte for byte are in `WIRE.md`, and
//! `tests/wire_vectors.json` holds them as numbers.

pub mod body;
mod bot;
pub mod channel;
pub mod crypto;
pub mod dm;
mod error;
pub mod image;
pub mod invite;
pub mod panel;
pub mod realtime;
pub mod sealed;
mod seed;
mod senders;
mod session;
pub mod suggest;
#[cfg(test)]
mod test_server;
mod tls;
#[cfg(feature = "voice")]
pub mod voice;
#[cfg(feature = "voice")]
mod voice_location;
#[cfg(all(test, feature = "voice"))]
mod voice_live_test;

pub use bot::{Bot, BotMessage, DEFAULT_POLL, HandlerOutput, PanelOptions};
pub use channel::{ChannelMessage, ChannelReader};
pub use dm::{DirectMessage, DirectMessages};
pub use error::{BotError, Result};
pub use invite::{Invite, parse_invite};
pub use seed::{create_seed, load_seed};
pub use session::{BotSession, JoinOptions, REQUEST_TIMEOUT, SessionConfig};
pub use suggest::{SuggestRequest, Suggestion};

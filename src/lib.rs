//! Write a bot for [Rift](https://joinrift.app) in Rust.
//!
//! The formats this crate must match byte for byte are in `WIRE.md`, and
//! `tests/wire_vectors.json` holds them as numbers.

pub mod body;
pub mod crypto;
mod error;
pub mod invite;
pub mod realtime;
pub mod sealed;
mod session;

pub use error::{BotError, Result};
pub use invite::{Invite, parse_invite};
pub use session::{BotSession, JoinOptions, REQUEST_TIMEOUT, SessionConfig};

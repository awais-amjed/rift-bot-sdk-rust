//! The one string an admin hands a bot.
//!
//! An invite packs the server's URL and the code together so whoever is
//! joining pastes one thing. Three shapes, all the same pair underneath
//! (`lib/data/invite_link.dart` in the app):
//!
//! - `<server-url>#<code>`, the plain form;
//! - `rift://join#<server-url>#<code>`, what a landing page hands the app;
//! - `https://joinrift.app/join#<server-url>#<code>`, the clickable one.
//!
//! This mirrors the app's parser rather than inventing a looser one: a third
//! reading of the format is a third thing that can disagree about where the
//! URL ends, and the failure is a bot that cannot join a server whose invite
//! works for everybody else.

/// A parsed invite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub server_url: String,
    pub invite_code: String,
}

fn make(server_url: &str, invite_code: &str) -> Option<Invite> {
    let url = server_url.trim().trim_end_matches('/');
    let code = invite_code.trim();
    if url.is_empty() || code.is_empty() {
        return None;
    }
    Some(Invite { server_url: url.to_string(), invite_code: code.to_string() })
}

/// Parse any of the three shapes. `None` if it is not a complete invite.
pub fn parse_invite(input: &str) -> Option<Invite> {
    let trimmed = input.trim();

    // A wrapper first: `<wrapper>#<server-url>#<code>`. Split the plain way
    // round and the last `#` would take the code off correctly but leave the
    // wrapper glued to the front of the server URL.
    let parts: Vec<&str> = trimmed.split('#').collect();
    if parts.len() == 3 && parts[1].starts_with("http") {
        return make(parts[1], parts[2]);
    }

    let sep = trimmed.rfind('#')?;
    if sep == 0 {
        return None;
    }
    make(&trimmed[..sep], &trimmed[sep + 1..])
}

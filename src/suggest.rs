//! Suggestions while somebody types a command (WIRE.md §7).
//!
//! A member's Rift asks as they type `/play thats so tr`, on this bot's own
//! topic, and the bot answers on theirs. Nothing is stored: both halves are
//! Realtime broadcasts, gone once delivered.
//!
//! **Who asked is a claim.** A broadcast carries no sender the server vouches
//! for, so `from` is whatever the asking client wrote. The worst a liar gets is
//! suggestions sent to somebody else, and that client drops them, because only
//! the asker knows the request's `id`. So the answer is safe to send; the
//! request is not safe to *act* on. A suggestion handler looks things up and
//! nothing more: the command itself arrives later, signed.

use serde::Serialize;
use serde_json::{Value, json};

/// The event a member's client sends to `user:<bot id>`.
pub const SUGGEST_EVENT: &str = "bot_suggest";
/// The event a bot answers with, on `user:<asker id>`.
pub const SUGGESTIONS_EVENT: &str = "bot_suggestions";
/// At most this many come back; a menu above a composer holds a handful.
pub const MAX_SUGGESTIONS: usize = 10;

const MAX_TEXT: usize = 200;
const MAX_LABEL: usize = 100;
const MAX_VALUE: usize = 500;

/// What somebody has typed so far, after the command's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuggestRequest {
    /// The client's own id for this request, echoed in the answer.
    pub id: String,
    /// Who says they asked. Not verified: see the top of this module.
    pub from: String,
    /// The channel they are typing in, as they say.
    pub channel_id: String,
    /// The command, lower case, without its slash: `play`.
    pub command: String,
    /// Everything after `/play `, as typed.
    pub text: String,
}

/// One row of the menu. Picking it sends `/<command> <value>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Suggestion {
    pub label: String,
    pub value: String,
}

impl Suggestion {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self { label: label.into(), value: value.into() }
    }
}

fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups.iter().zip([8, 4, 4, 4, 12]).all(|(g, len)| {
            g.len() == len && g.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        })
}

fn all_in(s: &str, len: std::ops::RangeInclusive<usize>, ok: impl Fn(char) -> bool) -> bool {
    len.contains(&s.len()) && s.chars().all(ok)
}

/// A request as it came off the wire, or `None` for anything malformed.
pub fn parse_suggest_request(payload: &Value) -> Option<SuggestRequest> {
    if payload["v"] != json!(1) {
        return None;
    }
    let id = payload["id"].as_str()?;
    let from = payload["from"].as_str()?;
    let channel = payload["channel"].as_str()?;
    let command = payload["command"].as_str()?;
    let text = payload["text"].as_str()?;
    let id_ok = all_in(id, 8..=64, |c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let command_ok = all_in(command, 1..=32, |c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
    });
    // Length in UTF-16 units, as the client counts it; never fewer than chars.
    let text_ok = text.encode_utf16().count() <= MAX_TEXT;
    if !(id_ok && command_ok && text_ok && is_uuid(from) && is_uuid(channel)) {
        return None;
    }
    Some(SuggestRequest {
        id: id.into(),
        from: from.into(),
        channel_id: channel.into(),
        command: command.into(),
        text: text.into(),
    })
}

/// What a handler returned, cut to what the wire allows: ten rows, each with
/// a label and a value of sane length. A row that does not fit is dropped
/// rather than cut, since a cut value would send something nobody picked.
pub fn clean_suggestions(items: &[Suggestion]) -> Vec<Suggestion> {
    items
        .iter()
        .filter(|s| {
            !s.label.trim().is_empty()
                && !s.value.trim().is_empty()
                && s.value.encode_utf16().count() <= MAX_VALUE
                && !s.value.contains('\n')
        })
        .take(MAX_SUGGESTIONS)
        .map(|s| Suggestion {
            label: if s.label.chars().count() > MAX_LABEL {
                format!("{}…", s.label.chars().take(MAX_LABEL - 1).collect::<String>())
            } else {
                s.label.clone()
            },
            value: s.value.clone(),
        })
        .collect()
}

/// The answer's payload.
pub fn suggestions_payload(request: &SuggestRequest, bot_id: &str, items: &[Suggestion]) -> Value {
    json!({ "v": 1, "id": request.id, "bot": bot_id, "items": clean_suggestions(items) })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID_A: &str = "11111111-2222-4333-8444-555555555555";
    const UUID_B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn request() -> Value {
        json!({ "v": 1, "id": "abcdEFGH_-12", "from": UUID_A, "channel": UUID_B,
                "command": "play", "text": "thats so tr" })
    }

    #[test]
    fn a_well_formed_request_is_read_as_sent() {
        let r = parse_suggest_request(&request()).unwrap();
        assert_eq!((r.id.as_str(), r.from.as_str()), ("abcdEFGH_-12", UUID_A));
        assert_eq!((r.channel_id.as_str(), r.command.as_str(), r.text.as_str()), (UUID_B, "play", "thats so tr"));
    }

    #[test]
    fn anything_malformed_is_no_request_at_all() {
        let bad = [
            ("v", json!(2)),
            ("id", json!("short")),
            ("id", json!("has space in it")),
            ("from", json!("not-a-uuid")),
            ("from", json!(UUID_B.to_uppercase())),
            ("channel", json!(7)),
            ("command", json!("Play")),
            ("command", json!("")),
            ("text", json!("x".repeat(201))),
        ];
        for (key, value) in bad {
            let mut payload = request();
            payload[key] = value.clone();
            assert!(parse_suggest_request(&payload).is_none(), "{key} = {value}");
        }
    }

    #[test]
    fn the_answer_keeps_ten_drops_the_unsendable_and_shortens_a_long_label() {
        let mut items: Vec<Suggestion> = (0..12).map(|i| Suggestion::new(format!("song {i}"), format!("v{i}"))).collect();
        items.insert(0, Suggestion::new("two lines", "a\nb"));
        items.insert(0, Suggestion::new(" ", "blank label"));
        items.insert(0, Suggestion::new("long", "x".repeat(501)));
        items.insert(0, Suggestion::new("y".repeat(150), "ok"));
        let cleaned = clean_suggestions(&items);
        assert_eq!(cleaned.len(), 10);
        assert_eq!(cleaned[0].label.chars().count(), 100);
        assert!(cleaned[0].label.ends_with('…'));
        assert_eq!(cleaned[1].value, "v0");
    }

    #[test]
    fn the_answer_echoes_the_request_id() {
        let r = parse_suggest_request(&request()).unwrap();
        let payload = suggestions_payload(&r, "bot-1", &[Suggestion::new("a", "b")]);
        assert_eq!(payload, json!({ "v": 1, "id": "abcdEFGH_-12", "bot": "bot-1",
                                    "items": [{ "label": "a", "value": "b" }] }));
    }
}

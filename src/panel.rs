//! Building a panel's blocks (WIRE.md §5).
//!
//! A block is plain JSON, and anything not in the vocabulary is not drawn:
//! a bot never controls a pixel, only a structure. These are shorthands for
//! the shapes Rift draws; a block written out by hand works the same.

use serde_json::{Value, json};

pub type Block = Value;

pub fn heading(text: &str) -> Block {
    json!({ "type": "heading", "text": text })
}

pub fn text(text: &str) -> Block {
    json!({ "type": "text", "text": text })
}

/// Key/value rows.
pub fn fields(items: &[(&str, &str)]) -> Block {
    let items: Vec<Value> = items.iter().map(|(label, value)| json!({ "label": label, "value": value })).collect();
    json!({ "type": "fields", "items": items })
}

/// A bar, `value` from 0 to 1, with a caption.
pub fn progress(value: f64, text: &str) -> Block {
    json!({ "type": "progress", "value": value.clamp(0.0, 1.0), "text": text })
}

pub fn divider() -> Block {
    json!({ "type": "divider" })
}

/// One button. A press arrives as a message whose `action_id` is `action`.
#[derive(Debug, Clone)]
pub struct Button {
    label: String,
    action: String,
    style: Option<&'static str>,
}

pub fn button(label: &str, action: &str) -> Button {
    Button { label: label.into(), action: action.into(), style: None }
}

impl Button {
    /// A weight, not a colour: a bot cannot paint a button into looking like
    /// part of Rift's own chrome.
    pub fn primary(mut self) -> Self {
        self.style = Some("primary");
        self
    }

    pub fn danger(mut self) -> Self {
        self.style = Some("danger");
        self
    }

    fn to_json(&self) -> Value {
        let mut item = json!({ "label": self.label, "action": self.action });
        if let Some(style) = self.style {
            item["style"] = json!(style);
        }
        item
    }
}

/// A row of buttons.
pub fn actions(buttons: &[Button]) -> Block {
    json!({ "type": "actions", "items": buttons.iter().map(Button::to_json).collect::<Vec<_>>() })
}

/// A menu. Picking `(label, value)` arrives as a press of `action` with
/// `action_value` set to `value`.
pub fn select(action: &str, placeholder: Option<&str>, options: &[(&str, &str)]) -> Block {
    let options: Vec<Value> = options.iter().map(|(label, value)| json!({ "label": label, "value": value })).collect();
    let mut block = json!({ "type": "select", "action": action, "options": options });
    if let Some(text) = placeholder {
        block["text"] = json!(text);
    }
    block
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_are_the_shapes_wire_md_names() {
        assert_eq!(progress(1.7, "x")["value"], json!(1.0));
        assert_eq!(
            actions(&[button("Skip", "skip").primary(), button("Stop", "stop")]),
            json!({ "type": "actions", "items": [
                { "label": "Skip", "action": "skip", "style": "primary" },
                { "label": "Stop", "action": "stop" },
            ]})
        );
        assert_eq!(
            select("volume", None, &[("Loud", "100")]),
            json!({ "type": "select", "action": "volume", "options": [{ "label": "Loud", "value": "100" }] })
        );
    }
}

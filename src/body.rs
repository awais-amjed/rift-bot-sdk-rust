//! The tagged JSON a sealed message's plaintext actually is.

const BODY_TAG: &str = "rift.msg";

/// Unwrap the body. Anything that is not the tagged object is a message from
/// before the shape existed and is its own text, which is why this never fails
/// on content it does not recognise.
pub fn decode_body(plaintext: &str) -> String {
    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(plaintext)
        && map.get("t").and_then(|t| t.as_str()) == Some(BODY_TAG) {
            return map.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
        }
    plaintext.to_string()
}

pub fn encode_body(text: &str) -> String {
    serde_json::json!({ "t": BODY_TAG, "v": 1, "text": text }).to_string()
}

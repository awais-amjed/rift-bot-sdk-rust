/// Anything the server refused, or a request that never got an answer.
#[derive(Debug, Clone)]
pub struct BotError {
    pub message: String,
    /// The HTTP status, when there was one.
    ///
    /// The polling loop has to tell two failures apart: a session that expired
    /// an hour after it was minted (401), recovered by signing again and
    /// nobody's business, and a request refused on its merits, which a bot
    /// must hear about. Treating the second as the first meant a bot that
    /// could not do what it was asked looked like a bot nobody had asked.
    pub status: Option<u16>,
}

impl BotError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            status: None,
        }
    }

    pub(crate) fn with_status(message: impl Into<String>, status: u16) -> Self {
        Self {
            message: message.into(),
            status: Some(status),
        }
    }
}

impl std::fmt::Display for BotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for BotError {}

pub type Result<T, E = BotError> = std::result::Result<T, E>;

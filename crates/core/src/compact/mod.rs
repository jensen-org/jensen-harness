//! The token saving layer. Every tool result passes through here before it
//! reaches the transcript, so the model is only sent what it needs. The
//! filters are deterministic and never call a model. Nothing is lost for
//! good: a result that was cut is teed to a raw store first, and the model
//! can ask for any slice of it back.

pub mod generic;
pub mod outline;
pub mod render;
pub mod shell;
pub mod store;

#[derive(Debug, Clone, thiserror::Error)]
pub enum CompactError {
    #[error("the raw output store failed: {0}")]
    Store(String),
    #[error("{0} is not a handle this session issued")]
    UnknownHandle(String),
    #[error("the source could not be parsed: {0}")]
    Parse(String),
}

#[must_use]
pub fn tokens(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton()
        .encode_with_special_tokens(text)
        .len()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compacted {
    pub text: String,
    pub raw_tokens: usize,
    pub sent_tokens: usize,
}

impl Compacted {
    #[must_use]
    pub fn new(raw: &str, text: String) -> Self {
        let sent_tokens = tokens(&text);
        Self {
            raw_tokens: tokens(raw),
            sent_tokens,
            text,
        }
    }

    #[must_use]
    pub fn saved_tokens(&self) -> usize {
        self.raw_tokens.saturating_sub(self.sent_tokens)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Savings {
    pub raw_tokens: usize,
    pub sent_tokens: usize,
}

impl Savings {
    pub fn record(&mut self, compacted: &Compacted) {
        self.raw_tokens += compacted.raw_tokens;
        self.sent_tokens += compacted.sent_tokens;
    }
}

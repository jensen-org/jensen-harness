//! Every model vendor is config, never a Rust type per vendor. A provider is
//! an `id` and an endpoint, reached through the genai crate: Anthropic,
//! OpenAI compatible endpoints, gateways and the other vendors.

pub mod config;
pub mod genai;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ContentBlock {
    Text(String),
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
    /// A provider block this crate does not interpret, kept so it can be
    /// sent back unchanged (Anthropic `thinking` and `redacted_thinking`,
    /// whose signatures the API validates on the next request).
    Opaque(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Turn {
    #[must_use]
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            content: vec![ContentBlock::Text(text.into())],
        }
    }

    #[must_use]
    pub fn plain_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub model: String,
    pub system: String,
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    PauseTurn,
}

/// Token counts in separate buckets: `input_tokens` excludes the cache
/// buckets, so the four fields add up to the whole bill.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub content: Vec<ContentBlock>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("{provider}'s API key is missing: {hint}")]
    MissingApiKey { provider: String, hint: String },
    #[error(transparent)]
    Secret(#[from] crate::secrets::SecretError),
    #[error("{provider} returned an error: {message}")]
    Upstream { provider: String, message: String },
    #[error("the provider's response could not be read: {0}")]
    Malformed(String),
    #[error("the provider config is invalid: {0}")]
    Config(String),
    #[error("no provider is configured with the id {0}")]
    UnknownProvider(String),
}

#[async_trait]
pub trait Provider: Send + Sync + std::fmt::Debug {
    fn id(&self) -> &str;

    /// # Errors
    /// Returns [`ProviderError`] when the upstream call fails or the
    /// response cannot be parsed into a known shape.
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;
}

pub(crate) const MAX_OUTPUT_TOKENS: u32 = 16_384;

//! Every model vendor is config, never a Rust type per vendor. A
//! [`Provider`] is identified by an `id` and reached through a `transport`;
//! this phase ships one transport, `anthropic`, directly over the Messages
//! API. An `openai_compatible` transport (OpenAI, Gemini's compatible
//! endpoint, Mistral, DeepSeek, Qwen, GLM, Moonshot, and gateways such as
//! OpenRouter or a LiteLLM proxy) is deliberately deferred to a later phase,
//! so no vendor-per-role claim gets hardcoded before a real eval exists.

pub mod anthropic;

use async_trait::async_trait;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone)]
pub struct CompletionRequest {
    pub system: String,
    pub turns: Vec<Turn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub text: String,
    pub stop_reason: StopReason,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("{provider}'s API key is unset: set the {env_var} environment variable")]
    MissingApiKey { provider: String, env_var: String },
    #[error("{provider} returned an error: {message}")]
    Upstream { provider: String, message: String },
    #[error("the provider's response could not be read: {0}")]
    Malformed(String),
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn id(&self) -> &str;

    /// # Errors
    /// Returns [`ProviderError`] when the provider's key is unset, the
    /// upstream call fails, or the response cannot be parsed.
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError>;
}

/// Reads an API key only from the named environment variable, and fails
/// loudly rather than silently if a provider any role references has none
/// set.
///
/// # Errors
/// Returns [`ProviderError::MissingApiKey`] when `env_var` is unset.
pub fn require_api_key(provider: &str, env_var: &str) -> Result<String, ProviderError> {
    std::env::var(env_var).map_err(|_missing| ProviderError::MissingApiKey {
        provider: provider.to_string(),
        env_var: env_var.to_string(),
    })
}

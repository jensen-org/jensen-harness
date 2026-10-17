//! The Anthropic Messages API, called directly: no SDK, no wrapped CLI.
//! Kept as its own transport, not folded into a generic OpenAI-compatible
//! one, because later phases hang Anthropic-specific mechanics off it
//! (prompt cache breakpoints, `output_config.effort`, task budgets) that a
//! generic transport cannot express. This phase is deliberately
//! non-streaming; streaming is a later phase's concern, not a simplification
//! this one hides.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{CompletionRequest, CompletionResponse, Provider, ProviderError, Role, StopReason};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicProvider {
    id: String,
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

impl std::fmt::Debug for AnthropicProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnthropicProvider")
            .field("id", &self.id)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .finish()
    }
}

impl AnthropicProvider {
    /// Takes the resolved key directly rather than an environment variable
    /// name: reading the environment is the caller's job (see
    /// [`super::require_api_key`]), so this type never touches process
    /// environment itself, in production code or in a test.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Debug, Serialize)]
struct WireMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Serialize)]
struct MessagesRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    system: &'a str,
    messages: Vec<WireMessage<'a>>,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let messages: Vec<WireMessage<'_>> = request
            .turns
            .iter()
            .map(|turn| WireMessage {
                role: role_name(turn.role),
                content: &turn.text,
            })
            .collect();
        let body = MessagesRequest {
            model: &self.model,
            max_tokens: DEFAULT_MAX_TOKENS,
            system: &request.system,
            messages,
        };
        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let response = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|error| ProviderError::Upstream {
                provider: self.id.clone(),
                message: error.to_string(),
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_error| String::new());
            return Err(ProviderError::Upstream {
                provider: self.id.clone(),
                message: format!("{status}: {text}"),
            });
        }
        let parsed: MessagesResponse = response
            .json()
            .await
            .map_err(|error| ProviderError::Malformed(error.to_string()))?;
        let text = parsed
            .content
            .into_iter()
            .find(|block| block.kind == "text")
            .and_then(|block| block.text)
            .ok_or_else(|| {
                ProviderError::Malformed("no text content block in the response".to_string())
            })?;
        let stop_reason = match parsed.stop_reason.as_deref() {
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        Ok(CompletionResponse { text, stop_reason })
    }
}

//! Every provider except native Anthropic goes through the genai crate:
//! OpenAI compatible endpoints and gateways, Gemini, DeepSeek, Mistral and
//! the rest. The endpoint and key come from config, so genai never guesses
//! a vendor from a model name.

use ::genai::adapter::AdapterKind;
use ::genai::chat::{
    CacheControl, ChatMessage, ChatOptions, ChatRequest, ContentPart, MessageContent,
    StopReason as GenaiStop, Tool, ToolCall, ToolResponse, Usage as GenaiUsage,
};
use ::genai::resolver::{AuthData, Endpoint};
use ::genai::{Client, ModelIden, ServiceTarget};
use async_trait::async_trait;
use serde_json::{Value, json};

use super::{
    CompletionRequest, CompletionResponse, ContentBlock, MAX_OUTPUT_TOKENS, Provider,
    ProviderError, Role, StopReason, Turn, Usage,
};

const REASONING_KIND: &str = "genai_reasoning";
const THOUGHT_SIGNATURE_KIND: &str = "genai_thought_signature";

#[must_use]
pub fn parse_adapter(name: &str) -> Option<AdapterKind> {
    Some(match name {
        "anthropic" => AdapterKind::Anthropic,
        "openai" | "openai_compatible" => AdapterKind::OpenAI,
        "openai_resp" => AdapterKind::OpenAIResp,
        "gemini" => AdapterKind::Gemini,
        "fireworks" => AdapterKind::Fireworks,
        "together" => AdapterKind::Together,
        "groq" => AdapterKind::Groq,
        "aihubmix" => AdapterKind::Aihubmix,
        "mimo" => AdapterKind::Mimo,
        "moonshot" => AdapterKind::Moonshot,
        "nebius" => AdapterKind::Nebius,
        "xai" => AdapterKind::Xai,
        "deepseek" => AdapterKind::DeepSeek,
        "zai" => AdapterKind::Zai,
        "aliyun" => AdapterKind::Aliyun,
        "baidu" => AdapterKind::Baidu,
        "cohere" => AdapterKind::Cohere,
        "ollama" => AdapterKind::Ollama,
        "openrouter" => AdapterKind::OpenRouter,
        "minimax" => AdapterKind::MiniMax,
        _ => return None,
    })
}

pub struct GenaiProvider {
    id: String,
    adapter: AdapterKind,
    base_url: String,
    api_key: String,
    client: Client,
}

impl std::fmt::Debug for GenaiProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GenaiProvider")
            .field("id", &self.id)
            .field("adapter", &self.adapter)
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .finish()
    }
}

impl GenaiProvider {
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        adapter: AdapterKind,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let mut base_url = base_url.into();
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        Self {
            id: id.into(),
            adapter,
            base_url,
            api_key: api_key.into(),
            client: Client::default(),
        }
    }

    fn target(&self, model: &str) -> ServiceTarget {
        ServiceTarget {
            endpoint: Endpoint::from_owned(self.base_url.as_str()),
            auth: AuthData::from_single(self.api_key.as_str()),
            model: ModelIden::new(self.adapter, model),
        }
    }
}

fn messages_to_genai(turns: &[Turn]) -> Vec<ChatMessage> {
    let mut messages = Vec::new();
    for turn in turns {
        match turn.role {
            Role::User => {
                let responses: Vec<ToolResponse> = turn
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => Some(ToolResponse::new(
                            tool_use_id.as_str(),
                            if *is_error && !content.starts_with("ERROR") {
                                format!("ERROR: {content}")
                            } else {
                                content.clone()
                            },
                        )),
                        _ => None,
                    })
                    .collect();
                if !responses.is_empty() {
                    messages.push(ChatMessage::tool(MessageContent::from_tool_responses(
                        responses,
                    )));
                }
                let text = turn.plain_text();
                if !text.is_empty() {
                    messages.push(ChatMessage::user(text));
                }
            }
            Role::Assistant => {
                let parts: Vec<ContentPart> =
                    turn.content.iter().filter_map(part_to_genai).collect();
                if !parts.is_empty() {
                    messages.push(ChatMessage::assistant(MessageContent::from_parts(parts)));
                }
            }
        }
    }
    messages
}

fn part_to_genai(block: &ContentBlock) -> Option<ContentPart> {
    match block {
        ContentBlock::Text(text) if !text.is_empty() => Some(ContentPart::Text(text.clone())),
        ContentBlock::ToolUse { id, name, input } => Some(ContentPart::ToolCall(ToolCall {
            call_id: id.clone(),
            fn_name: name.clone(),
            fn_arguments: input.clone(),
            thought_signatures: None,
        })),
        ContentBlock::Opaque(value) => {
            let kind = value.get("type").and_then(Value::as_str)?;
            let payload = value.get("value").and_then(Value::as_str)?.to_string();
            match kind {
                REASONING_KIND => Some(ContentPart::ReasoningContent(payload)),
                THOUGHT_SIGNATURE_KIND => Some(ContentPart::ThoughtSignature(payload)),
                _ => None,
            }
        }
        _ => None,
    }
}

fn opaque(kind: &str, value: String) -> ContentBlock {
    ContentBlock::Opaque(json!({"type": kind, "value": value}))
}

fn stop_reason_from_genai(reason: Option<&GenaiStop>) -> Result<StopReason, ProviderError> {
    match reason {
        Some(GenaiStop::Completed(_)) => Ok(StopReason::EndTurn),
        Some(GenaiStop::MaxTokens(_)) => Ok(StopReason::MaxTokens),
        Some(GenaiStop::ToolCall(_)) => Ok(StopReason::ToolUse),
        Some(GenaiStop::ContentFilter(_)) => Ok(StopReason::Refusal),
        Some(GenaiStop::StopSequence(_)) => Ok(StopReason::StopSequence),
        Some(GenaiStop::Other(other)) if other == "refusal" => Ok(StopReason::Refusal),
        Some(GenaiStop::Other(other)) if other == "pause_turn" => Ok(StopReason::PauseTurn),
        Some(GenaiStop::Other(other)) => Err(ProviderError::Malformed(format!(
            "unknown stop reason {other:?}"
        ))),
        None => Err(ProviderError::Malformed(
            "the response has no stop reason".to_string(),
        )),
    }
}

fn count(value: Option<i32>, what: &str) -> Result<u64, ProviderError> {
    match value {
        None => Ok(0),
        Some(number) => u64::try_from(number)
            .map_err(|_negative| ProviderError::Malformed(format!("{what} is negative"))),
    }
}

fn usage_from_genai(usage: &GenaiUsage) -> Result<Usage, ProviderError> {
    let prompt = count(usage.prompt_tokens, "prompt_tokens")?;
    let (cached, written) = match &usage.prompt_tokens_details {
        Some(details) => (
            count(details.cached_tokens, "cached_tokens")?,
            count(details.cache_creation_tokens, "cache_creation_tokens")?,
        ),
        None => (0, 0),
    };
    let input_tokens = prompt
        .checked_sub(cached)
        .and_then(|rest| rest.checked_sub(written))
        .ok_or_else(|| {
            ProviderError::Malformed("cache tokens exceed the prompt tokens".to_string())
        })?;
    Ok(Usage {
        input_tokens,
        output_tokens: count(usage.completion_tokens, "completion_tokens")?,
        cache_read_tokens: cached,
        cache_write_tokens: written,
    })
}

#[async_trait]
impl Provider for GenaiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn reports_cache(&self) -> bool {
        self.adapter == AdapterKind::Anthropic
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let tools: Vec<Tool> = request
            .tools
            .iter()
            .map(|tool| {
                Tool::new(tool.name.as_str())
                    .with_description(tool.description.as_str())
                    .with_schema(tool.input_schema.clone())
            })
            .collect();
        let mut messages = messages_to_genai(&request.turns);
        let mut chat = if self.adapter == AdapterKind::Anthropic {
            messages.insert(
                0,
                ChatMessage::system(request.system.as_str())
                    .with_options(CacheControl::Ephemeral1h),
            );
            if let Some(last) = messages.last_mut() {
                last.options = Some(CacheControl::Ephemeral5m.into());
            }
            ChatRequest::new(messages)
        } else {
            ChatRequest::new(messages).with_system(request.system.as_str())
        };
        if !tools.is_empty() {
            chat = chat.with_tools(tools);
        }
        let options = ChatOptions::default().with_max_tokens(MAX_OUTPUT_TOKENS);
        let response = self
            .client
            .exec_chat(self.target(&request.model), chat, Some(&options))
            .await
            .map_err(|error| ProviderError::Upstream {
                provider: self.id.clone(),
                message: error.to_string(),
            })?;
        let stop_reason = stop_reason_from_genai(response.stop_reason.as_ref())?;
        let usage = usage_from_genai(&response.usage)?;
        let mut content = Vec::new();
        if let Some(reasoning) = response.reasoning_content {
            content.push(opaque(REASONING_KIND, reasoning));
        }
        for part in response.content {
            match part {
                ContentPart::Text(text) => content.push(ContentBlock::Text(text)),
                ContentPart::ThoughtSignature(signature) => {
                    content.push(opaque(THOUGHT_SIGNATURE_KIND, signature));
                }
                ContentPart::ToolCall(call) => {
                    for signature in call.thought_signatures.into_iter().flatten() {
                        content.push(opaque(THOUGHT_SIGNATURE_KIND, signature));
                    }
                    content.push(ContentBlock::ToolUse {
                        id: call.call_id,
                        name: call.fn_name,
                        input: call.fn_arguments,
                    });
                }
                _ => {}
            }
        }
        Ok(CompletionResponse {
            content,
            stop_reason,
            usage,
        })
    }
}

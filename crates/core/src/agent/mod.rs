//! The agent loop: an append-only transcript, a provider, a tool set. A
//! prompt runs model calls until the model ends its turn or the turn cap
//! stops it. No other harness's own agent loop is driven here; this is the
//! one place a model gets called.

use crate::compact::Savings;
use crate::providers::{
    CompletionRequest, ContentBlock, Provider, ProviderError, Role, StopReason, Turn, Usage,
};
use crate::toolset::{ToolReply, ToolSet, ToolSetError};

const SYSTEM_PROMPT: &str = "You are jensen-harness, a software engineering agent working in one worktree through tools. Be direct. Large files come back as outlines: ask for a symbol or a line range. Cut output ends with an expand handle. Prefer grep and glob to reading whole files. Edit with a find string that matches once.";

pub const DEFAULT_MAX_TURNS: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStop {
    EndTurn,
    StopSequence,
    MaxTokens,
    Refusal,
    TurnLimit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptOutcome {
    pub stop: PromptStop,
    pub model_calls: u32,
    pub usage: Usage,
    pub savings: Savings,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolSetError),
}

pub struct Session {
    model: String,
    turns: Vec<Turn>,
    tools: ToolSet,
    max_turns: u32,
}

impl Session {
    #[must_use]
    pub fn new(model: impl Into<String>, tools: ToolSet, max_turns: u32) -> Self {
        Self {
            model: model.into(),
            turns: Vec::new(),
            tools,
            max_turns,
        }
    }

    /// Runs `message` to the end of the model's turn. `max_turns` bounds the
    /// model calls one prompt may spend.
    ///
    /// # Errors
    /// [`AgentError::Provider`] when a model call fails and
    /// [`AgentError::Tool`] when the harness cannot run a tool. In both
    /// cases the transcript stays valid for another prompt.
    pub async fn prompt(
        &mut self,
        provider: &dyn Provider,
        message: &str,
    ) -> Result<PromptOutcome, AgentError> {
        self.turns.push(Turn::text(Role::User, message));
        let mut usage = Usage::default();
        let mut savings = Savings::default();
        let mut model_calls = 0u32;
        loop {
            if model_calls >= self.max_turns {
                return Ok(self.outcome(PromptStop::TurnLimit, model_calls, usage, savings));
            }
            let response = provider
                .complete(CompletionRequest {
                    model: self.model.clone(),
                    system: SYSTEM_PROMPT.to_string(),
                    turns: self.turns.clone(),
                    tools: ToolSet::specs(),
                })
                .await?;
            model_calls += 1;
            add_usage(&mut usage, response.usage);
            self.turns.push(Turn {
                role: Role::Assistant,
                content: response.content.clone(),
            });
            let stop = match response.stop_reason {
                StopReason::ToolUse => {
                    self.run_tools(&response.content, &mut savings).await?;
                    continue;
                }
                StopReason::PauseTurn => continue,
                StopReason::EndTurn => PromptStop::EndTurn,
                StopReason::StopSequence => PromptStop::StopSequence,
                StopReason::MaxTokens => PromptStop::MaxTokens,
                StopReason::Refusal => PromptStop::Refusal,
            };
            return Ok(self.outcome(stop, model_calls, usage, savings));
        }
    }

    fn outcome(
        &self,
        stop: PromptStop,
        model_calls: u32,
        usage: Usage,
        savings: Savings,
    ) -> PromptOutcome {
        PromptOutcome {
            stop,
            model_calls,
            usage,
            savings,
        }
    }

    /// Every tool result for one reply goes back in a single user turn. If
    /// the harness fails partway, the calls it did not finish are answered
    /// with an error result first, so the transcript never holds a tool
    /// call without its result.
    async fn run_tools(
        &mut self,
        content: &[ContentBlock],
        savings: &mut Savings,
    ) -> Result<(), AgentError> {
        let calls: Vec<(&String, &String, &serde_json::Value)> = content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some((id, name, input)),
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            return Err(ProviderError::Malformed(
                "the model stopped for a tool call but sent none".to_string(),
            )
            .into());
        }
        let mut results = Vec::with_capacity(calls.len());
        for (position, (id, name, input)) in calls.iter().enumerate() {
            match self.tools.invoke(name, input).await {
                Ok(reply) => {
                    savings.raw_tokens += reply.raw_tokens;
                    savings.sent_tokens += reply.sent_tokens;
                    results.push(result_block(id, reply));
                }
                Err(error) => {
                    for (skipped_id, _, _) in calls.iter().skip(position) {
                        results.push(ContentBlock::ToolResult {
                            tool_use_id: (*skipped_id).clone(),
                            content: "the harness could not run this tool".to_string(),
                            is_error: true,
                        });
                    }
                    self.turns.push(Turn {
                        role: Role::User,
                        content: results,
                    });
                    return Err(error.into());
                }
            }
        }
        self.turns.push(Turn {
            role: Role::User,
            content: results,
        });
        Ok(())
    }

    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
}

fn result_block(id: &str, reply: ToolReply) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: reply.text,
        is_error: reply.is_error,
    }
}

fn add_usage(total: &mut Usage, step: Usage) {
    total.input_tokens += step.input_tokens;
    total.output_tokens += step.output_tokens;
    total.cache_read_tokens += step.cache_read_tokens;
    total.cache_write_tokens += step.cache_write_tokens;
}

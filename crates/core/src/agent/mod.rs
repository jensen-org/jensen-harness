//! The agent loop: an append-only transcript, a provider, a tool set and a
//! budget. A prompt runs model calls until the model ends its turn, the turn
//! cap stops it, or a hard budget cap refuses the next call. No other
//! harness's own agent loop is driven here; this is the one place a model
//! gets called.

use std::time::Instant;

use serde_json::json;

use crate::budget::estimate::reservation;
use crate::budget::{BudgetError, SessionBudget, soft_threshold};
use crate::compact::Savings;
use crate::providers::{
    CompletionRequest, ContentBlock, Provider, ProviderError, Role, StopReason, Turn, Usage,
};
use crate::store::{
    EventKind, NewEvent, NewScope, Refusal, ReserveOutcome, ReserveRequest, ScopeKind, Store,
    StoreError,
};
use crate::toolset::{ToolReply, ToolSet, ToolSetError};

const SYSTEM_PROMPT: &str = "You are jensen-harness, a software engineering agent working in one worktree through tools. Be direct. Large files come back as outlines: ask for a symbol or a line range. Cut output ends with an expand handle. Prefer grep and glob to reading whole files. Edit with a find string that matches once.";

pub const DEFAULT_MAX_TURNS: u32 = 20;
const RESERVATION_TTL_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptStop {
    EndTurn,
    StopSequence,
    MaxTokens,
    Refusal,
    TurnLimit,
    BudgetExhausted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptOutcome {
    pub stop: PromptStop,
    pub model_calls: u32,
    pub usage: Usage,
    pub cost_micro_usd: u64,
    pub savings: Savings,
}

#[derive(Default)]
struct Tally {
    model_calls: u32,
    usage: Usage,
    cost_micro_usd: u64,
    savings: Savings,
}

impl Tally {
    fn finish(&self, stop: PromptStop) -> PromptOutcome {
        PromptOutcome {
            stop,
            model_calls: self.model_calls,
            usage: self.usage,
            cost_micro_usd: self.cost_micro_usd,
            savings: self.savings,
        }
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum AgentError {
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Tool(#[from] ToolSetError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
}

/// What a session is, and what gets journaled when it is created.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub session_id: String,
    /// `None` gives the session a private run with no caps of its own.
    pub run_id: Option<String>,
    /// `provider_id/model_id`; the part after the first slash is what the
    /// provider is asked for.
    pub model_address: String,
    pub max_turns: u32,
    pub budget: SessionBudget,
    pub price_version: String,
}

pub struct Session {
    config: SessionConfig,
    model: String,
    turns: Vec<Turn>,
    tools: ToolSet,
    store: Store,
    soft_notified: bool,
}

impl Session {
    /// Opens the session's scopes in the ledger and journals its creation.
    ///
    /// # Errors
    /// [`AgentError::Store`] when the ledger or journal refuses, for example
    /// a `run_id` that does not exist.
    pub async fn create(
        store: Store,
        config: SessionConfig,
        tools: ToolSet,
    ) -> Result<Self, AgentError> {
        let run_id = match &config.run_id {
            Some(run_id) => {
                store.usage(run_id).await?;
                run_id.clone()
            }
            None => {
                let private = format!("run-{}", config.session_id);
                store
                    .open_scope(NewScope {
                        id: private.clone(),
                        parent_id: None,
                        kind: ScopeKind::Run,
                        caps: crate::budget::Caps::default(),
                        soft_percent: config.budget.soft_percent,
                    })
                    .await?;
                private
            }
        };
        store
            .open_scope(NewScope {
                id: config.session_id.clone(),
                parent_id: Some(run_id.clone()),
                kind: ScopeKind::Session,
                caps: config.budget.caps,
                soft_percent: config.budget.soft_percent,
            })
            .await?;
        store
            .append(NewEvent {
                session_id: config.session_id.clone(),
                parent_id: None,
                kind: EventKind::SessionCreated,
                payload: json!({
                    "run_id": run_id,
                    "model_address": config.model_address,
                    "worktree": tools.root(),
                    "max_turns": config.max_turns,
                    "budget": config.budget,
                    "price_version": config.price_version,
                }),
            })
            .await?;
        let model = config
            .model_address
            .split_once('/')
            .map_or(config.model_address.as_str(), |(_, model)| model)
            .to_string();
        Ok(Self {
            config,
            model,
            turns: Vec::new(),
            tools,
            store,
            soft_notified: false,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.config.session_id
    }

    /// Runs `message` to the end of the model's turn. `max_turns` bounds the
    /// model calls one prompt may spend.
    ///
    /// # Errors
    /// [`AgentError::Provider`] when a model call fails,
    /// [`AgentError::Tool`] when the harness cannot run a tool, and
    /// [`AgentError::Store`] when the ledger or journal fails. In every case
    /// the transcript stays valid for another prompt.
    pub async fn prompt(
        &mut self,
        provider: &dyn Provider,
        message: &str,
    ) -> Result<PromptOutcome, AgentError> {
        self.turns.push(Turn::text(Role::User, message));
        let mut tally = Tally::default();
        loop {
            if tally.model_calls >= self.config.max_turns {
                return Ok(tally.finish(PromptStop::TurnLimit));
            }
            let request = CompletionRequest {
                model: self.model.clone(),
                system: SYSTEM_PROMPT.to_string(),
                turns: self.turns.clone(),
                tools: ToolSet::specs(),
            };
            let call_id = ulid::Ulid::generate().to_string();
            let (tokens, held_cost) = reservation(&request, self.config.budget.price.as_ref())?;
            let reserved = self
                .store
                .reserve(ReserveRequest {
                    scope_id: self.config.session_id.clone(),
                    call_id: call_id.clone(),
                    tokens,
                    cost_micro_usd: held_cost,
                    ttl_ms: RESERVATION_TTL_MS,
                })
                .await?;
            if let ReserveOutcome::Refused(refusal) = reserved {
                self.journal_exhausted(&refusal).await?;
                return Ok(tally.finish(PromptStop::BudgetExhausted));
            }
            let started = Instant::now();
            let response = match provider.complete(request).await {
                Ok(response) => response,
                Err(error) => {
                    self.store
                        .release(
                            &call_id,
                            Some(self.event(
                                EventKind::ModelFailed,
                                json!({"call_id": call_id, "error": error.to_string()}),
                            )),
                        )
                        .await?;
                    return Err(error.into());
                }
            };
            let call_cost = match &self.config.budget.price {
                Some(price) => price.cost(&response.usage)?,
                None => 0,
            };
            self.store
                .settle(
                    &call_id,
                    response.usage.total_tokens()?,
                    call_cost,
                    Some(self.event(
                        EventKind::ModelResponded,
                        json!({
                            "call_id": call_id,
                            "model": self.config.model_address,
                            "content": response.content,
                            "stop_reason": response.stop_reason,
                            "usage": response.usage,
                            "cost_micro_usd": call_cost,
                            "price_version": self.config.price_version,
                            "latency_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                        }),
                    )),
                )
                .await?;
            tally.model_calls += 1;
            add_usage(&mut tally.usage, response.usage);
            tally.cost_micro_usd = tally.cost_micro_usd.saturating_add(call_cost);
            self.turns.push(Turn {
                role: Role::Assistant,
                content: response.content.clone(),
            });
            let stop = match response.stop_reason {
                StopReason::ToolUse => {
                    self.run_tools(&response.content, &mut tally.savings)
                        .await?;
                    continue;
                }
                StopReason::PauseTurn => continue,
                StopReason::EndTurn => PromptStop::EndTurn,
                StopReason::StopSequence => PromptStop::StopSequence,
                StopReason::MaxTokens => PromptStop::MaxTokens,
                StopReason::Refusal => PromptStop::Refusal,
            };
            return Ok(tally.finish(stop));
        }
    }

    fn event(&self, kind: EventKind, payload: serde_json::Value) -> NewEvent {
        NewEvent {
            session_id: self.config.session_id.clone(),
            parent_id: None,
            kind,
            payload,
        }
    }

    async fn journal_exhausted(&self, refusal: &Refusal) -> Result<(), AgentError> {
        self.store
            .append(self.event(
                EventKind::BudgetExhausted,
                json!({
                    "scope_id": refusal.scope_id,
                    "limit": format!("{:?}", refusal.limit),
                    "cap": refusal.cap,
                    "in_use": refusal.in_use,
                    "requested": refusal.requested,
                }),
            ))
            .await?;
        Ok(())
    }

    /// Words for the model once any scope passes its soft threshold, or
    /// `None`. Said once per session.
    async fn soft_cap_note(&mut self) -> Result<Option<String>, AgentError> {
        if self.soft_notified {
            return Ok(None);
        }
        for scope in self.store.usage(&self.config.session_id).await? {
            let tokens = scope
                .caps
                .max_tokens
                .map(|cap| (cap, scope.used_tokens(), "token"));
            let cost = scope
                .caps
                .max_cost_micro_usd
                .map(|cap| (cap, scope.used_cost_micro_usd(), "cost"));
            for (cap, used, what) in [tokens, cost].into_iter().flatten() {
                if used >= soft_threshold(cap, scope.soft_percent) {
                    self.soft_notified = true;
                    let percent = u128::from(used) * 100 / u128::from(cap).max(1);
                    return Ok(Some(format!(
                        "[budget] {percent}% of the {what} budget is used. Finish the task, or say what is left."
                    )));
                }
            }
        }
        Ok(None)
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
        let mut results = Vec::with_capacity(calls.len() + 1);
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
        if let Some(note) = self.soft_cap_note().await? {
            results.push(ContentBlock::Text(note));
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

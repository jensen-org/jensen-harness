//! The agent loop: an append-only transcript, a provider, a tool set, a
//! budget and a journal. A prompt runs model calls until the model ends its
//! turn, the turn cap stops it, or a hard budget cap refuses the next call.
//! Every step is written to the journal before the loop moves on, so a
//! prompt that was cut short can be picked up again without repeating a
//! completed model call or a completed tool. No other harness's own agent
//! loop is driven here; this is the one place a model gets called.

mod recovery;

use std::collections::HashMap;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
use recovery::{OpenPrompt, PendingTools, ToolProgress};

pub use recovery::Recovered;

const SYSTEM_PROMPT: &str = "You are jensen-harness, a software engineering agent working in one worktree through tools. Be direct. Large files come back as outlines: ask for a symbol or a line range. Cut output ends with an expand handle. Prefer grep and glob to reading whole files. Edit with a find string that matches once.";

pub const DEFAULT_MAX_TURNS: u32 = 20;
const RESERVATION_TTL_MS: u64 = 10 * 60 * 1000;

/// These tools change nothing, so running one twice is harmless.
const READ_ONLY_TOOLS: [&str; 4] = ["read", "glob", "grep", "expand"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Default)]
pub(crate) struct Tally {
    pub model_calls: u32,
    pub usage: Usage,
    pub cost_micro_usd: u64,
    pub savings: Savings,
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

    pub(crate) fn add_usage(&mut self, step: Usage) {
        self.usage.input_tokens += step.input_tokens;
        self.usage.output_tokens += step.output_tokens;
        self.usage.cache_read_tokens += step.cache_read_tokens;
        self.usage.cache_write_tokens += step.cache_write_tokens;
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
    #[error("a prompt is still open on this session: resume it first")]
    PromptOpen,
    #[error("this session has no interrupted prompt to resume")]
    NothingToResume,
    #[error("no session {0} in the journal")]
    UnknownSession(String),
    #[error("the journal cannot be replayed: {0}")]
    Journal(String),
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
    run_id: String,
    model: String,
    turns: Vec<Turn>,
    tools: ToolSet,
    store: Store,
    soft_notified: bool,
    responses: u32,
    cache_alerted: bool,
    open: Option<OpenPrompt>,
    last_event: Option<String>,
}

/// Below this share of the prompt read from cache, the cache is not doing
/// its job.
const CACHE_LOW_PERMILLE: u64 = 800;

/// Parts per thousand of the prompt that came from cache, or `None` when
/// there was no prompt to measure.
fn cache_hit_permille(usage: &Usage) -> Option<u64> {
    let prompt = usage
        .input_tokens
        .checked_add(usage.cache_read_tokens)?
        .checked_add(usage.cache_write_tokens)?;
    if prompt == 0 {
        return None;
    }
    let permille = u128::from(usage.cache_read_tokens) * 1000 / u128::from(prompt);
    u64::try_from(permille).ok()
}

fn model_id(address: &str) -> String {
    address
        .split_once('/')
        .map_or(address, |(_, model)| model)
        .to_string()
}

fn interrupted(tool: &str) -> ToolReply {
    let text = format!(
        "ERROR {tool}: this call was interrupted by a restart and may have partly run, or may still be running. Check the current state before trying again."
    );
    let tokens = crate::compact::tokens(&text);
    ToolReply {
        text,
        is_error: true,
        raw_tokens: tokens,
        sent_tokens: tokens,
    }
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
        let created = store
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
        Ok(Self {
            model: model_id(&config.model_address),
            config,
            run_id,
            turns: Vec::new(),
            tools,
            store,
            soft_notified: false,
            responses: 0,
            cache_alerted: false,
            open: None,
            last_event: Some(created.id),
        })
    }

    /// Rebuilds a session from its journal. Model calls that were in flight
    /// when the process stopped are released: their reservation is freed and
    /// nothing is charged for them.
    ///
    /// # Errors
    /// [`AgentError::Store`] when the ledger or journal refuses.
    pub async fn restore(
        store: Store,
        recovered: Recovered,
        tools: ToolSet,
    ) -> Result<Self, AgentError> {
        let Recovered {
            config, rebuilt, ..
        } = recovered;
        let mut last_event = rebuilt.last_event;
        for call_id in store.open_reservations(&config.session_id).await? {
            let released = store
                .release(
                    &call_id,
                    Some(NewEvent {
                        session_id: config.session_id.clone(),
                        parent_id: last_event.clone(),
                        kind: EventKind::ModelFailed,
                        payload: json!({"call_id": call_id, "error": "interrupted by a restart"}),
                    }),
                )
                .await?;
            last_event = released.map(|event| event.id).or(last_event);
        }
        let run_id = config.run_id.clone().ok_or_else(|| {
            AgentError::Journal(format!("session {} has no run", config.session_id))
        })?;
        Ok(Self {
            model: model_id(&config.model_address),
            config,
            run_id,
            turns: rebuilt.turns,
            tools,
            store,
            soft_notified: rebuilt.soft_notified,
            responses: rebuilt.responses,
            cache_alerted: rebuilt.cache_alerted,
            open: rebuilt.open,
            last_event,
        })
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.config.session_id
    }

    #[must_use]
    pub fn model_address(&self) -> &str {
        &self.config.model_address
    }

    #[must_use]
    pub fn has_open_prompt(&self) -> bool {
        self.open.is_some()
    }

    /// Runs `message` to the end of the model's turn. `max_turns` bounds the
    /// model calls one prompt may spend.
    ///
    /// # Errors
    /// [`AgentError::PromptOpen`] while an earlier prompt is unfinished,
    /// [`AgentError::Provider`] when a model call fails,
    /// [`AgentError::Tool`] when the harness cannot run a tool, and
    /// [`AgentError::Store`] when the ledger or journal fails. An unfinished
    /// prompt can be continued with [`Session::resume`].
    pub async fn prompt(
        &mut self,
        provider: &dyn Provider,
        message: &str,
    ) -> Result<PromptOutcome, AgentError> {
        if self.open.is_some() {
            return Err(AgentError::PromptOpen);
        }
        self.record(EventKind::PromptStarted, json!({"message": message}))
            .await?;
        self.turns.push(Turn::text(Role::User, message));
        self.drive(provider, OpenPrompt::default()).await
    }

    /// Continues a prompt that was cut short, by a provider error, a harness
    /// failure or a restart, from exactly where the journal says it stopped.
    ///
    /// # Errors
    /// [`AgentError::NothingToResume`] when no prompt is open, otherwise the
    /// same as [`Session::prompt`].
    pub async fn resume(&mut self, provider: &dyn Provider) -> Result<PromptOutcome, AgentError> {
        let open = self.open.take().ok_or(AgentError::NothingToResume)?;
        self.drive(provider, open).await
    }

    async fn drive(
        &mut self,
        provider: &dyn Provider,
        mut open: OpenPrompt,
    ) -> Result<PromptOutcome, AgentError> {
        let result = self.drive_inner(provider, &mut open).await;
        self.open = result.is_err().then_some(open);
        result
    }

    async fn drive_inner(
        &mut self,
        provider: &dyn Provider,
        open: &mut OpenPrompt,
    ) -> Result<PromptOutcome, AgentError> {
        loop {
            if let Some(stop) = open.ended {
                return self.finish_prompt(open, stop).await;
            }
            if open.pending.is_some() {
                self.run_pending_tools(open).await?;
                continue;
            }
            if open.tally.model_calls >= self.config.max_turns {
                return self.finish_prompt(open, PromptStop::TurnLimit).await;
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
                return self.finish_prompt(open, PromptStop::BudgetExhausted).await;
            }
            let started = Instant::now();
            let response = match provider.complete(request).await {
                Ok(response) => response,
                Err(error) => {
                    let event = self.new_event(
                        EventKind::ModelFailed,
                        json!({"call_id": call_id, "error": error.to_string()}),
                    );
                    let stored = self.store.release(&call_id, Some(event)).await?;
                    self.last_event = stored.map(|event| event.id).or(self.last_event.take());
                    return Err(error.into());
                }
            };
            let call_cost = match &self.config.budget.price {
                Some(price) => price.cost(&response.usage)?,
                None => 0,
            };
            let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let permille = cache_hit_permille(&response.usage);
            let event = self.new_event(
                EventKind::ModelResponded,
                json!({
                    "call_id": call_id,
                    "run_id": self.run_id,
                    "model": self.config.model_address,
                    "content": response.content,
                    "stop_reason": response.stop_reason,
                    "usage": response.usage,
                    "cost_micro_usd": call_cost,
                    "price_version": self.config.price_version,
                    "latency_ms": latency_ms,
                    "cache_hit_permille": permille,
                }),
            );
            let stored = self
                .store
                .settle(
                    &call_id,
                    response.usage.total_tokens()?,
                    call_cost,
                    Some(event),
                )
                .await?;
            self.last_event = stored.map(|event| event.id).or(self.last_event.take());
            tracing::info!(
                session_id = %self.config.session_id,
                run_id = %self.run_id,
                model = %self.config.model_address,
                call_id = %call_id,
                input_tokens = response.usage.input_tokens,
                output_tokens = response.usage.output_tokens,
                cache_read_tokens = response.usage.cache_read_tokens,
                cache_write_tokens = response.usage.cache_write_tokens,
                cost_micro_usd = call_cost,
                price_version = %self.config.price_version,
                latency_ms,
                stop_reason = ?response.stop_reason,
                cache_hit_permille = ?permille,
                "model call settled"
            );
            self.check_cache(provider, &call_id, permille).await?;
            self.responses += 1;
            open.tally.model_calls += 1;
            open.tally.add_usage(response.usage);
            open.tally.cost_micro_usd = open.tally.cost_micro_usd.saturating_add(call_cost);
            self.turns.push(Turn {
                role: Role::Assistant,
                content: response.content.clone(),
            });
            match response.stop_reason {
                StopReason::ToolUse => {
                    open.pending = Some(PendingTools {
                        content: response.content,
                        progress: HashMap::new(),
                    });
                }
                StopReason::PauseTurn => {}
                StopReason::EndTurn => open.ended = Some(PromptStop::EndTurn),
                StopReason::StopSequence => open.ended = Some(PromptStop::StopSequence),
                StopReason::MaxTokens => open.ended = Some(PromptStop::MaxTokens),
                StopReason::Refusal => open.ended = Some(PromptStop::Refusal),
            }
        }
    }

    async fn finish_prompt(
        &mut self,
        open: &OpenPrompt,
        stop: PromptStop,
    ) -> Result<PromptOutcome, AgentError> {
        let outcome = open.tally.finish(stop);
        self.record(
            EventKind::PromptFinished,
            json!({
                "stop": outcome.stop,
                "model_calls": outcome.model_calls,
                "usage": outcome.usage,
                "cost_micro_usd": outcome.cost_micro_usd,
                "savings": outcome.savings,
            }),
        )
        .await?;
        Ok(outcome)
    }

    /// The first call writes the cache, so only a later one can fall short.
    /// Said once per session, and never for a provider that does not report
    /// cache tokens.
    async fn check_cache(
        &mut self,
        provider: &dyn Provider,
        call_id: &str,
        permille: Option<u64>,
    ) -> Result<(), AgentError> {
        let Some(permille) = permille else {
            return Ok(());
        };
        if !provider.reports_cache()
            || self.responses == 0
            || self.cache_alerted
            || permille >= CACHE_LOW_PERMILLE
        {
            return Ok(());
        }
        self.cache_alerted = true;
        tracing::warn!(
            session_id = %self.config.session_id,
            call_id,
            cache_hit_permille = permille,
            "less than 80% of the prompt was read from cache"
        );
        self.record(
            EventKind::CacheLow,
            json!({"call_id": call_id, "cache_hit_permille": permille}),
        )
        .await
    }

    fn new_event(&self, kind: EventKind, payload: Value) -> NewEvent {
        NewEvent {
            session_id: self.config.session_id.clone(),
            parent_id: self.last_event.clone(),
            kind,
            payload,
        }
    }

    async fn record(&mut self, kind: EventKind, payload: Value) -> Result<(), AgentError> {
        let stored = self.store.append(self.new_event(kind, payload)).await?;
        self.last_event = Some(stored.id);
        Ok(())
    }

    async fn journal_exhausted(&mut self, refusal: &Refusal) -> Result<(), AgentError> {
        self.record(
            EventKind::BudgetExhausted,
            json!({
                "scope_id": refusal.scope_id,
                "limit": format!("{:?}", refusal.limit),
                "cap": refusal.cap,
                "in_use": refusal.in_use,
                "requested": refusal.requested,
            }),
        )
        .await
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

    /// Answers the tool calls of the last model reply, in order, and commits
    /// the answers as one user turn. A call the journal shows as finished is
    /// not run again. One it shows as started and never finished may have
    /// partly run: a tool that only reads is run again, any other is answered
    /// with an error saying so, never repeated.
    ///
    /// If the harness itself fails partway, the calls it did not finish are
    /// answered with an error result, so the transcript never holds a tool
    /// call without its result.
    async fn run_pending_tools(&mut self, open: &mut OpenPrompt) -> Result<(), AgentError> {
        let Some(pending) = open.pending.take() else {
            return Ok(());
        };
        let calls: Vec<(String, String, Value)> = pending
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();
        if calls.is_empty() {
            return Err(ProviderError::Malformed(
                "the model stopped for a tool call but sent none".to_string(),
            )
            .into());
        }
        let mut progress = pending.progress;
        let mut results = Vec::with_capacity(calls.len() + 1);
        for (position, (id, name, input)) in calls.iter().enumerate() {
            let prior = progress.remove(id);
            let reply = match prior {
                Some(ToolProgress::Finished(reply)) => reply,
                Some(ToolProgress::Started) if !READ_ONLY_TOOLS.contains(&name.as_str()) => {
                    let reply = interrupted(name);
                    self.record_tool_finished(open, id, &reply).await?;
                    reply
                }
                prior => {
                    if prior.is_none() {
                        self.record(
                            EventKind::ToolStarted,
                            json!({"tool_use_id": id, "name": name, "input": input}),
                        )
                        .await?;
                    }
                    match self.tools.invoke(name, input).await {
                        Ok(reply) => {
                            self.record_tool_finished(open, id, &reply).await?;
                            reply
                        }
                        Err(error) => {
                            for (skipped_id, _, _) in calls.iter().skip(position) {
                                results.push(ContentBlock::ToolResult {
                                    tool_use_id: skipped_id.clone(),
                                    content: "the harness could not run this tool".to_string(),
                                    is_error: true,
                                });
                            }
                            self.commit_results(results).await?;
                            return Err(error.into());
                        }
                    }
                }
            };
            results.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: reply.text,
                is_error: reply.is_error,
            });
        }
        if let Some(note) = self.soft_cap_note().await? {
            results.push(ContentBlock::Text(note));
        }
        self.commit_results(results).await
    }

    async fn record_tool_finished(
        &mut self,
        open: &mut OpenPrompt,
        id: &str,
        reply: &ToolReply,
    ) -> Result<(), AgentError> {
        open.tally.savings.raw_tokens += reply.raw_tokens;
        open.tally.savings.sent_tokens += reply.sent_tokens;
        self.record(
            EventKind::ToolFinished,
            json!({
                "tool_use_id": id,
                "text": reply.text,
                "is_error": reply.is_error,
                "raw_tokens": reply.raw_tokens,
                "sent_tokens": reply.sent_tokens,
            }),
        )
        .await
    }

    async fn commit_results(&mut self, results: Vec<ContentBlock>) -> Result<(), AgentError> {
        self.record(EventKind::TurnResults, json!({"results": results}))
            .await?;
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

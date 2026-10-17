//! Rebuilding a session from its journal. Replay is a pure function over the
//! events: the same list always gives the same transcript and the same
//! answer to "what was this prompt doing when it stopped".

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{AgentError, PromptStop, SessionConfig, Tally};
use crate::budget::SessionBudget;
use crate::compact::Savings;
use crate::providers::{ContentBlock, Role, StopReason, Turn, Usage};
use crate::store::{Event, EventKind, Store};
use crate::toolset::ToolReply;

#[derive(Debug, Clone)]
pub(super) enum ToolProgress {
    Started,
    Finished(ToolReply),
}

#[derive(Debug, Clone)]
pub(super) struct PendingTools {
    pub content: Vec<ContentBlock>,
    pub progress: HashMap<String, ToolProgress>,
}

/// A prompt that has started and not finished.
#[derive(Debug, Clone, Default)]
pub(super) struct OpenPrompt {
    pub tally: Tally,
    pub pending: Option<PendingTools>,
    /// The model said it was done, but `prompt.finished` was never written.
    pub ended: Option<PromptStop>,
}

#[derive(Deserialize)]
struct Created {
    run_id: String,
    model_address: String,
    worktree: PathBuf,
    max_turns: u32,
    budget: SessionBudget,
    price_version: String,
}

#[derive(Deserialize)]
struct Started {
    message: String,
}

#[derive(Deserialize)]
struct Responded {
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
    usage: Usage,
    cost_micro_usd: u64,
}

#[derive(Deserialize)]
struct ToolStarted {
    tool_use_id: String,
}

#[derive(Deserialize)]
struct ToolFinished {
    tool_use_id: String,
    text: String,
    is_error: bool,
    raw_tokens: usize,
    sent_tokens: usize,
}

#[derive(Deserialize)]
struct TurnResults {
    results: Vec<ContentBlock>,
}

pub(super) struct Rebuilt {
    pub turns: Vec<Turn>,
    pub open: Option<OpenPrompt>,
    pub soft_notified: bool,
    pub last_event: Option<String>,
}

/// A session as its journal describes it, before it has a provider or tools.
pub struct Recovered {
    pub config: SessionConfig,
    pub worktree: PathBuf,
    pub(super) rebuilt: Rebuilt,
}

impl Recovered {
    #[must_use]
    pub fn has_open_prompt(&self) -> bool {
        self.rebuilt.open.is_some()
    }

    /// # Errors
    /// [`AgentError::UnknownSession`] when the journal has nothing for the
    /// id and [`AgentError::Journal`] when it cannot be replayed.
    pub async fn load(store: &Store, session_id: &str) -> Result<Self, AgentError> {
        let events = store.events(session_id).await?;
        if events.is_empty() {
            return Err(AgentError::UnknownSession(session_id.to_string()));
        }
        replay(session_id, &events)
    }
}

fn payload<T: DeserializeOwned>(event: &Event) -> Result<T, AgentError> {
    serde_json::from_value(event.payload.clone()).map_err(|error| {
        AgentError::Journal(format!("{} {}: {error}", event.kind.as_str(), event.id))
    })
}

fn terminal_stop(reason: StopReason) -> Option<PromptStop> {
    match reason {
        StopReason::EndTurn => Some(PromptStop::EndTurn),
        StopReason::StopSequence => Some(PromptStop::StopSequence),
        StopReason::MaxTokens => Some(PromptStop::MaxTokens),
        StopReason::Refusal => Some(PromptStop::Refusal),
        StopReason::ToolUse | StopReason::PauseTurn => None,
    }
}

fn open_of<'a>(
    open: &'a mut Option<OpenPrompt>,
    event: &Event,
) -> Result<&'a mut OpenPrompt, AgentError> {
    open.as_mut().ok_or_else(|| {
        AgentError::Journal(format!(
            "{} {} appears outside a prompt",
            event.kind.as_str(),
            event.id
        ))
    })
}

fn replay(session_id: &str, events: &[Event]) -> Result<Recovered, AgentError> {
    let mut created: Option<Created> = None;
    let mut turns: Vec<Turn> = Vec::new();
    let mut open: Option<OpenPrompt> = None;
    let mut soft_notified = false;
    let mut last_event = None;
    for event in events {
        last_event = Some(event.id.clone());
        match event.kind {
            EventKind::SessionCreated => created = Some(payload(event)?),
            EventKind::PromptStarted => {
                let started: Started = payload(event)?;
                turns.push(Turn::text(Role::User, started.message));
                open = Some(OpenPrompt::default());
            }
            EventKind::ModelResponded => {
                let responded: Responded = payload(event)?;
                let prompt = open_of(&mut open, event)?;
                turns.push(Turn {
                    role: Role::Assistant,
                    content: responded.content.clone(),
                });
                prompt.tally.model_calls += 1;
                prompt.tally.add_usage(responded.usage);
                prompt.tally.cost_micro_usd = prompt
                    .tally
                    .cost_micro_usd
                    .saturating_add(responded.cost_micro_usd);
                prompt.ended = terminal_stop(responded.stop_reason);
                prompt.pending =
                    (responded.stop_reason == StopReason::ToolUse).then(|| PendingTools {
                        content: responded.content,
                        progress: HashMap::new(),
                    });
            }
            EventKind::ToolStarted => {
                let started: ToolStarted = payload(event)?;
                if let Some(pending) = open_of(&mut open, event)?.pending.as_mut() {
                    pending
                        .progress
                        .entry(started.tool_use_id)
                        .or_insert(ToolProgress::Started);
                }
            }
            EventKind::ToolFinished => {
                let finished: ToolFinished = payload(event)?;
                let prompt = open_of(&mut open, event)?;
                prompt.tally.savings = Savings {
                    raw_tokens: prompt.tally.savings.raw_tokens + finished.raw_tokens,
                    sent_tokens: prompt.tally.savings.sent_tokens + finished.sent_tokens,
                };
                if let Some(pending) = prompt.pending.as_mut() {
                    pending.progress.insert(
                        finished.tool_use_id,
                        ToolProgress::Finished(ToolReply {
                            text: finished.text,
                            is_error: finished.is_error,
                            raw_tokens: finished.raw_tokens,
                            sent_tokens: finished.sent_tokens,
                        }),
                    );
                }
            }
            EventKind::TurnResults => {
                let results: TurnResults = payload(event)?;
                soft_notified |= results.results.iter().any(|block| {
                    matches!(block, ContentBlock::Text(text) if text.starts_with("[budget]"))
                });
                turns.push(Turn {
                    role: Role::User,
                    content: results.results,
                });
                open_of(&mut open, event)?.pending = None;
            }
            EventKind::PromptFinished => open = None,
            EventKind::ModelFailed | EventKind::BudgetExhausted | EventKind::CacheLow => {}
        }
    }
    let created = created.ok_or_else(|| {
        AgentError::Journal(format!("session {session_id} has no session.created event"))
    })?;
    Ok(Recovered {
        config: SessionConfig {
            session_id: session_id.to_string(),
            run_id: Some(created.run_id),
            model_address: created.model_address,
            max_turns: created.max_turns,
            budget: created.budget,
            price_version: created.price_version,
        },
        worktree: created.worktree,
        rebuilt: Rebuilt {
            turns,
            open,
            soft_notified,
            last_event,
        },
    })
}

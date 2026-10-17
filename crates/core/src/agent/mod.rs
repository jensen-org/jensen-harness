//! The agent loop: an append-only transcript, a provider, done. No other
//! harness's own agent loop is ever driven here; this is the one place a
//! model actually gets called.

use crate::providers::{CompletionRequest, Provider, ProviderError, Role, StopReason, Turn, Usage};

const SYSTEM_PROMPT: &str = "You are jensen-harness, a software engineering agent. Be direct.";

pub struct Session {
    model: String,
    turns: Vec<Turn>,
}

impl Session {
    #[must_use]
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            turns: Vec::new(),
        }
    }

    /// Appends `message` as a user turn, runs it against `provider` to
    /// completion, appends the assistant's reply and returns the stop reason
    /// with the tokens it cost.
    ///
    /// # Errors
    /// Propagates whatever [`ProviderError`] the provider call returns.
    pub async fn prompt(
        &mut self,
        provider: &dyn Provider,
        message: &str,
    ) -> Result<(StopReason, Usage), ProviderError> {
        self.turns.push(Turn::text(Role::User, message));
        let response = provider
            .complete(CompletionRequest {
                model: self.model.clone(),
                system: SYSTEM_PROMPT.to_string(),
                turns: self.turns.clone(),
                tools: Vec::new(),
            })
            .await?;
        self.turns.push(Turn {
            role: Role::Assistant,
            content: response.content,
        });
        Ok((response.stop_reason, response.usage))
    }

    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
}

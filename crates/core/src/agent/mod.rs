//! The agent loop: an append-only transcript, a provider, done. No other
//! harness's own agent loop is ever driven here; this is the one place a
//! model actually gets called.

use crate::providers::{CompletionRequest, Provider, ProviderError, Role, StopReason, Turn};

const SYSTEM_PROMPT: &str = "You are jensen-harness, a software engineering agent. Be direct.";

/// One session's transcript. Turns are only ever appended, never rewritten,
/// since editing earlier turns invalidates provider-side state (such as
/// cached prompt prefixes or preserved thinking) that this phase does not
/// yet manage but a later phase will.
pub struct Session {
    turns: Vec<Turn>,
}

impl Session {
    #[must_use]
    pub fn new() -> Self {
        Self { turns: Vec::new() }
    }

    /// Appends `message` as a user turn, runs it against `provider` to
    /// completion, appends the assistant's reply, and returns the stop
    /// reason.
    ///
    /// # Errors
    /// Propagates whatever [`ProviderError`] the provider call returns.
    pub async fn prompt(
        &mut self,
        provider: &dyn Provider,
        message: &str,
    ) -> Result<StopReason, ProviderError> {
        self.turns.push(Turn {
            role: Role::User,
            text: message.to_string(),
        });
        let response = provider
            .complete(CompletionRequest {
                system: SYSTEM_PROMPT.to_string(),
                turns: self.turns.clone(),
            })
            .await?;
        self.turns.push(Turn {
            role: Role::Assistant,
            text: response.text,
        });
        Ok(response.stop_reason)
    }

    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

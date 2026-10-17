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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::Session;
    use crate::providers::{
        CompletionRequest, CompletionResponse, Provider, ProviderError, Role, StopReason,
    };
    use async_trait::async_trait;

    struct Echo;

    #[async_trait]
    impl Provider for Echo {
        fn id(&self) -> &str {
            "echo"
        }

        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            let last = request
                .turns
                .last()
                .map(|turn| turn.text.clone())
                .unwrap_or_default();
            Ok(CompletionResponse {
                text: format!("echo: {last}"),
                stop_reason: StopReason::EndTurn,
            })
        }
    }

    #[tokio::test]
    async fn prompt_appends_both_turns_in_order() {
        let mut session = Session::new();
        let stop_reason = session.prompt(&Echo, "hello").await.unwrap();
        assert_eq!(stop_reason, StopReason::EndTurn);
        let [first, second] = session.turns() else {
            panic!("expected exactly two turns, got {}", session.turns().len());
        };
        assert_eq!(first.role, Role::User);
        assert_eq!(first.text, "hello");
        assert_eq!(second.role, Role::Assistant);
        assert_eq!(second.text, "echo: hello");
    }
}

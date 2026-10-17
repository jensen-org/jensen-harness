//! Fakes shared across more than one test file. A fake lives here only once
//! two or more files need it; a fake only one file needs stays local to it.

use async_trait::async_trait;
use jensen_harness_core::providers::{
    CompletionRequest, CompletionResponse, ContentBlock, Provider, ProviderError, StopReason, Usage,
};
use jensen_harness_core::sandbox::{
    Confined, EnforcementReport, Sandbox, SandboxError, SandboxPolicy,
};

/// Echoes the last user turn back, prefixed, so a test can assert on
/// exactly what reached the provider without a real model call.
#[derive(Debug)]
pub struct Echo;

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
            .map(|turn| turn.plain_text())
            .unwrap_or_default();
        Ok(CompletionResponse {
            content: vec![ContentBlock::Text(format!("echo: {last}"))],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
        })
    }
}

/// Passes a command straight through with no OS confinement at all, for a
/// test that is about something other than any one backend's own
/// filesystem or network enforcement (those have their own real,
/// platform-specific tests), such as the environment `shell::run` builds
/// for the child it spawns.
pub struct NoopSandbox;

impl Sandbox for NoopSandbox {
    fn confine(
        &self,
        _policy: &SandboxPolicy,
        program: &str,
        args: &[String],
    ) -> Result<Confined, SandboxError> {
        Ok(Confined {
            program: program.to_string(),
            args: args.to_vec(),
            report: EnforcementReport {
                backend: "noop",
                filesystem_rules_applied: 0,
                network_tier: "full",
                unsupported: Vec::new(),
            },
        })
    }
}

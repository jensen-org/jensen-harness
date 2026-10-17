//! Fakes shared across more than one test file. A fake lives here only once
//! two or more files need it; a fake only one file needs stays local to it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use async_trait::async_trait;
use jensen_harness_core::agent::{Session, SessionConfig};
use jensen_harness_core::budget::{BudgetSettings, SessionBudget};
use jensen_harness_core::compact::store::RawStore;
use jensen_harness_core::providers::{
    CompletionRequest, CompletionResponse, ContentBlock, Provider, ProviderError, StopReason, Usage,
};
use jensen_harness_core::sandbox::{
    Confined, EnforcementReport, Sandbox, SandboxError, SandboxPolicy,
};
use jensen_harness_core::store::Store;
use jensen_harness_core::tools::Worktree;
use jensen_harness_core::toolset::ToolSet;
use serde_json::{Value, json};

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

/// Refuses every command, as a host with no sandbox backend would.
pub struct UnavailableSandbox;

impl Sandbox for UnavailableSandbox {
    fn confine(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[String],
    ) -> Result<Confined, SandboxError> {
        Err(SandboxError::Unavailable(
            "no backend in this test".to_string(),
        ))
    }
}

/// A tool set over `root` whose shell runs unconfined, for tests about the
/// loop and the tools rather than about any one sandbox backend.
pub fn tool_set(root: &Path) -> ToolSet {
    tool_set_with(root, Box::new(NoopSandbox))
}

pub fn tool_set_with(root: &Path, sandbox: Box<dyn Sandbox>) -> ToolSet {
    let worktree = Worktree::open(root).unwrap();
    let policy = SandboxPolicy::confined_to(worktree.root());
    let store = RawStore::new(policy.scratch.join("raw"));
    ToolSet::new(worktree, sandbox, policy, store)
}

pub fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input,
    }
}

pub fn reply(content: Vec<ContentBlock>, stop_reason: StopReason) -> CompletionResponse {
    CompletionResponse {
        content,
        stop_reason,
        usage: Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 100,
            cache_write_tokens: 1,
        },
    }
}

pub fn calls_tool(id: &str, name: &str, input: Value) -> CompletionResponse {
    reply(vec![tool_use(id, name, input)], StopReason::ToolUse)
}

pub fn says(text: &str) -> CompletionResponse {
    reply(
        vec![ContentBlock::Text(text.to_string())],
        StopReason::EndTurn,
    )
}

/// Plays back a fixed list of replies, then repeats `fallback` if one was
/// given and fails otherwise. Every request it received stays inspectable.
#[derive(Debug)]
pub struct Scripted {
    script: Mutex<VecDeque<Result<CompletionResponse, ProviderError>>>,
    fallback: Option<CompletionResponse>,
    requests: Mutex<Vec<CompletionRequest>>,
    reports_cache: bool,
}

impl Scripted {
    pub fn new(script: Vec<Result<CompletionResponse, ProviderError>>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            fallback: None,
            requests: Mutex::new(Vec::new()),
            reports_cache: false,
        }
    }

    /// Makes this provider claim it reports prompt cache tokens.
    pub fn reporting_cache(mut self) -> Self {
        self.reports_cache = true;
        self
    }

    pub fn of(replies: Vec<CompletionResponse>) -> Self {
        Self::new(replies.into_iter().map(Ok).collect())
    }

    pub fn repeating(reply: CompletionResponse) -> Self {
        Self {
            fallback: Some(reply),
            ..Self::new(Vec::new())
        }
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn request(&self, index: usize) -> CompletionRequest {
        self.requests.lock().unwrap().get(index).unwrap().clone()
    }
}

#[async_trait]
impl Provider for Scripted {
    fn id(&self) -> &str {
        "scripted"
    }

    fn reports_cache(&self) -> bool {
        self.reports_cache
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.requests.lock().unwrap().push(request);
        let next = self.script.lock().unwrap().pop_front();
        match (next, &self.fallback) {
            (Some(result), _) => result,
            (None, Some(fallback)) => Ok(fallback.clone()),
            (None, None) => Err(ProviderError::Malformed("the script ran out".to_string())),
        }
    }
}

pub fn path_input(path: &str) -> Value {
    json!({ "path": path })
}

/// `cargo test` does not build the binary and `CARGO_BIN_EXE_jensen-harness`
/// is only set inside the binary's own package, so it is built here, once
/// per test run, then located under the workspace's target directory.
pub fn binary_path() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "--quiet", "-p", "jensen-harness"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("cargo can be run to build the binary");
        assert!(
            status.success(),
            "building the jensen-harness binary failed"
        );
    });
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    for profile in ["debug", "release"] {
        let candidate = target.join(profile).join("jensen-harness");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "the jensen-harness binary was not found under {}",
        target.display()
    );
}

pub fn default_budget() -> SessionBudget {
    BudgetSettings::default()
        .resolve(None, "scripted/m")
        .unwrap()
}

pub fn session_config(max_turns: u32, budget: SessionBudget) -> SessionConfig {
    SessionConfig {
        session_id: format!("s-{}", uuid::Uuid::new_v4()),
        run_id: None,
        model_address: "scripted/m".to_string(),
        max_turns,
        budget,
        price_version: "test".to_string(),
    }
}

pub async fn session_over(store: Store, tools: ToolSet, config: SessionConfig) -> Session {
    Session::create(store, config, tools).await.unwrap()
}

/// A session over `root` with an in memory store and the built in budget.
pub async fn new_session(root: &Path, max_turns: u32) -> Session {
    session_over(
        Store::in_memory().unwrap(),
        tool_set(root),
        session_config(max_turns, default_budget()),
    )
    .await
}

/// A reply that reports exactly `tokens` input tokens and nothing else.
pub fn costing(tokens: u64, mut response: CompletionResponse) -> CompletionResponse {
    response.usage = Usage {
        input_tokens: tokens,
        ..Usage::default()
    };
    response
}

/// A reply with exactly this usage.
pub fn with_usage(mut response: CompletionResponse, usage: Usage) -> CompletionResponse {
    response.usage = usage;
    response
}

pub fn testkit_mcp_path() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "--quiet", "-p", "jensen-harness-testkit-mcp"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("cargo can be run to build the fixture server");
        assert!(status.success(), "building the fixture MCP server failed");
    });
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    for profile in ["debug", "release"] {
        let candidate = target.join(profile).join("jensen-testkit-mcp");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "the fixture MCP server was not found under {}",
        target.display()
    );
}

pub fn testkit_lsp_path() -> PathBuf {
    static BUILT: std::sync::Once = std::sync::Once::new();
    BUILT.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
        let status = std::process::Command::new(cargo)
            .args(["build", "--quiet", "-p", "jensen-harness-testkit-lsp"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("cargo can be run to build the fixture language server");
        assert!(
            status.success(),
            "building the fixture language server failed"
        );
    });
    let target = Path::new(env!("CARGO_MANIFEST_DIR")).join("target");
    for profile in ["debug", "release"] {
        let candidate = target.join(profile).join("jensen-testkit-lsp");
        if candidate.exists() {
            return candidate;
        }
    }
    panic!(
        "the fixture language server was not found under {}",
        target.display()
    );
}

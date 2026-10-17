//! What the model can call. Each tool is parsed, run, then compacted before
//! the reply goes back into the transcript.
//!
//! Failures split in two. A mistake the model made (an unknown tool, bad
//! input, a path outside the worktree, a find string that is not there)
//! comes back as an error reply it can read and correct. A failure of the
//! harness itself (the sandbox is unavailable, the raw store cannot be
//! written) is a typed error that ends the run.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::compact::render::{self, ReadArgs};
use crate::compact::store::RawStore;
use crate::compact::{CompactError, Compacted};
use crate::providers::ToolSpec;
use crate::sandbox::{self, Sandbox, SandboxPolicy};
use crate::tools::{ToolError, Worktree, edit, glob, grep, read, shell};

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolSetError {
    #[error("the sandbox could not run the command: {0}")]
    Sandbox(String),
    #[error("the harness could not run the tool: {0}")]
    Infrastructure(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReply {
    pub text: String,
    pub is_error: bool,
    pub raw_tokens: usize,
    pub sent_tokens: usize,
}

impl ToolReply {
    fn ok(compacted: Compacted) -> Self {
        Self {
            text: compacted.text,
            is_error: false,
            raw_tokens: compacted.raw_tokens,
            sent_tokens: compacted.sent_tokens,
        }
    }

    fn model_error(tool: &str, message: &str) -> Self {
        let text = format!("ERROR {tool}: {message}");
        let tokens = crate::compact::tokens(&text);
        Self {
            text,
            is_error: true,
            raw_tokens: tokens,
            sent_tokens: tokens,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    symbol: Option<String>,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    find: String,
    replace: String,
    #[serde(default)]
    replace_all: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobInput {
    pattern: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrepInput {
    pattern: String,
    path: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShellInput {
    command: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpandInput {
    handle: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

const EXPAND_DEFAULT_LIMIT: usize = 200;

pub struct ToolSet {
    worktree: Worktree,
    sandbox: Box<dyn Sandbox>,
    policy: SandboxPolicy,
    store: RawStore,
    shell_timeout: Duration,
    read_only: bool,
}

impl std::fmt::Debug for ToolSet {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ToolSet")
            .field("worktree", &self.worktree.root())
            .finish_non_exhaustive()
    }
}

fn parse<T: DeserializeOwned>(tool: &str, input: &Value) -> Result<T, ToolReply> {
    serde_json::from_value(input.clone())
        .map_err(|error| ToolReply::model_error(tool, &format!("invalid input: {error}")))
}

fn infrastructure(error: impl std::fmt::Display) -> ToolSetError {
    ToolSetError::Infrastructure(error.to_string())
}

impl ToolSet {
    /// Confines every tool to the worktree at `root`, with the platform's
    /// own sandbox for `shell`.
    ///
    /// # Errors
    /// [`ToolError::Io`] when `root` is missing or not a directory.
    pub fn for_worktree(root: &Path) -> Result<Self, ToolError> {
        let worktree = Worktree::open(root)?;
        let policy = SandboxPolicy::confined_to(worktree.root());
        let store = RawStore::new(
            std::env::temp_dir().join(format!("jensen-harness-raw-{}", uuid::Uuid::new_v4())),
        );
        Ok(Self::new(
            worktree,
            sandbox::platform_default(),
            policy,
            store,
        ))
    }

    #[must_use]
    pub fn new(
        worktree: Worktree,
        sandbox: Box<dyn Sandbox>,
        policy: SandboxPolicy,
        store: RawStore,
    ) -> Self {
        Self {
            worktree,
            sandbox,
            policy,
            store,
            shell_timeout: shell::DEFAULT_TIMEOUT,
            read_only: false,
        }
    }

    #[must_use]
    pub fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    pub async fn run_command(
        &self,
        command: &str,
        timeout: Duration,
    ) -> Result<shell::ShellResult, ToolSetError> {
        shell::run_with_timeout(self.sandbox.as_ref(), &self.policy, command, timeout)
            .await
            .map_err(|error| ToolSetError::Sandbox(error.to_string()))
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        self.worktree.root()
    }

    #[must_use]
    pub fn with_shell_timeout(mut self, timeout: Duration) -> Self {
        self.shell_timeout = timeout;
        self
    }

    /// The same list in the same order on every call, so the tool block
    /// stays byte identical and caches.
    #[must_use]
    pub fn specs() -> Vec<ToolSpec> {
        let spec = |name: &str, description: &str, schema: Value| ToolSpec {
            name: name.to_string(),
            description: description.to_string(),
            input_schema: schema,
        };
        vec![
            spec(
                "read",
                "Read a file. A large file returns its outline; pass symbol, or start_line and end_line, for code.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "symbol": {"type": "string"},
                    "start_line": {"type": "integer"},
                    "end_line": {"type": "integer"},
                }, "required": ["path"]}),
            ),
            spec(
                "edit",
                "Replace find with replace in a file. find must match once unless replace_all.",
                json!({"type": "object", "properties": {
                    "path": {"type": "string"},
                    "find": {"type": "string"},
                    "replace": {"type": "string"},
                    "replace_all": {"type": "boolean"},
                }, "required": ["path", "find", "replace"]}),
            ),
            spec(
                "glob",
                "List files by glob: * within a directory, ** across them. Honors .gitignore.",
                json!({"type": "object", "properties": {
                    "pattern": {"type": "string"},
                }, "required": ["pattern"]}),
            ),
            spec(
                "grep",
                "Regex search lines in a file or directory (default .). Honors .gitignore.",
                json!({"type": "object", "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string"},
                }, "required": ["pattern"]}),
            ),
            spec(
                "shell",
                "Run a command in the sandboxed worktree. Output is filtered; cut output ends with an expand handle.",
                json!({"type": "object", "properties": {
                    "command": {"type": "string"},
                }, "required": ["command"]}),
            ),
            spec(
                "expand",
                "Read lines of cut output by handle.",
                json!({"type": "object", "properties": {
                    "handle": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                }, "required": ["handle"]}),
            ),
        ]
    }

    /// # Errors
    /// [`ToolSetError`] for a failure of the harness itself. Everything the
    /// model got wrong is an `Ok` reply with `is_error` set.
    pub async fn invoke(&self, name: &str, input: &Value) -> Result<ToolReply, ToolSetError> {
        if self.read_only && matches!(name, "edit" | "shell") {
            return Ok(ToolReply::model_error(name, "this agent is read only"));
        }
        match name {
            "read" => self.read(input),
            "edit" => self.edit(input),
            "glob" => self.glob(input),
            "grep" => self.grep(input),
            "shell" => self.shell(input).await,
            "expand" => self.expand(input),
            other => Ok(ToolReply::model_error(other, "unknown tool")),
        }
    }

    fn read(&self, input: &Value) -> Result<ToolReply, ToolSetError> {
        let args: ReadInput = match parse("read", input) {
            Ok(args) => args,
            Err(reply) => return Ok(reply),
        };
        let content = match read::run(&self.worktree, &args.path) {
            Ok(content) => content,
            Err(error) => return Ok(ToolReply::model_error("read", &error.to_string())),
        };
        let read_args = ReadArgs {
            symbol: args.symbol,
            start_line: args.start_line,
            end_line: args.end_line,
        };
        render::read(&args.path, &content, &read_args)
            .map(ToolReply::ok)
            .map_err(infrastructure)
    }

    fn edit(&self, input: &Value) -> Result<ToolReply, ToolSetError> {
        let args: EditInput = match parse("edit", input) {
            Ok(args) => args,
            Err(reply) => return Ok(reply),
        };
        Ok(
            match edit::run(
                &self.worktree,
                &args.path,
                &args.find,
                &args.replace,
                args.replace_all,
            ) {
                Ok(outcome) => ToolReply::ok(render::edit(&args.path, &outcome)),
                Err(error) => ToolReply::model_error("edit", &error.to_string()),
            },
        )
    }

    fn glob(&self, input: &Value) -> Result<ToolReply, ToolSetError> {
        let args: GlobInput = match parse("glob", input) {
            Ok(args) => args,
            Err(reply) => return Ok(reply),
        };
        Ok(match glob::run(&self.worktree, &args.pattern) {
            Ok(paths) => ToolReply::ok(render::glob(&paths)),
            Err(error) => ToolReply::model_error("glob", &error.to_string()),
        })
    }

    fn grep(&self, input: &Value) -> Result<ToolReply, ToolSetError> {
        let args: GrepInput = match parse("grep", input) {
            Ok(args) => args,
            Err(reply) => return Ok(reply),
        };
        let path = args.path.as_deref().unwrap_or(".");
        Ok(match grep::run(&self.worktree, path, &args.pattern) {
            Ok(hits) => ToolReply::ok(render::grep(&hits)),
            Err(error) => ToolReply::model_error("grep", &error.to_string()),
        })
    }

    async fn shell(&self, input: &Value) -> Result<ToolReply, ToolSetError> {
        let args: ShellInput = match parse("shell", input) {
            Ok(args) => args,
            Err(reply) => return Ok(reply),
        };
        let result = shell::run_with_timeout(
            self.sandbox.as_ref(),
            &self.policy,
            &args.command,
            self.shell_timeout,
        )
        .await
        .map_err(|error| match error {
            ToolError::Sandbox(message) => ToolSetError::Sandbox(message),
            other => infrastructure(other),
        })?;
        render::shell(
            &args.command,
            result.exit_code,
            &result.stdout,
            &result.stderr,
            &self.store,
        )
        .map(ToolReply::ok)
        .map_err(infrastructure)
    }

    fn expand(&self, input: &Value) -> Result<ToolReply, ToolSetError> {
        let args: ExpandInput = match parse("expand", input) {
            Ok(args) => args,
            Err(reply) => return Ok(reply),
        };
        match render::expand(
            &self.store,
            &args.handle,
            args.offset.unwrap_or(0),
            args.limit.unwrap_or(EXPAND_DEFAULT_LIMIT),
        ) {
            Ok(compacted) => Ok(ToolReply::ok(compacted)),
            Err(error @ CompactError::UnknownHandle(_)) => {
                Ok(ToolReply::model_error("expand", &error.to_string()))
            }
            Err(error) => Err(infrastructure(error)),
        }
    }
}

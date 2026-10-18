use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use globset::{Glob, GlobSetBuilder};
use jensen_harness_core::compact::{outline::outline, tokens};
use jensen_harness_core::providers::{ContentBlock, ToolSpec};
use jensen_harness_core::store::EventKind;
use jensen_harness_core::toolset::Confiner;
use jensen_harness_core::toolset::{ProviderReply, Replay, ToolProvider, ToolSetError};
use jensen_harness_extensions::{LspDirectory, LspKind, LspRequest, McpProvider, SkillCatalog};
use serde_json::{Value, json};

use crate::error::RunError;
use crate::nodes::Values;
use crate::runtime::Shared;
use crate::spec::{AgentNode, FetchMode, FetchSource, FetchSpec, InheritMode, LspQuery, TreeView};

const DEFAULT_SOURCE_TOKENS: u64 = 2_000;
const DEFAULT_TOTAL_TOKENS: u64 = 8_000;
const MAX_FILES: usize = 100;
const MAX_FILE_BYTES: u64 = 256 * 1024;
pub const LAZY_TOOL: &str = "fetch_context";

pub struct ContextEnv {
    pub root: PathBuf,
    pub confiner: Confiner,
    pub skills: SkillCatalog,
    pub mcp: BTreeMap<String, Arc<McpProvider>>,
    pub lsp: Option<Arc<dyn LspDirectory>>,
}

pub fn label(spec: &FetchSpec) -> String {
    match &spec.source {
        FetchSource::Skill { name } => format!("skill:{name}"),
        FetchSource::Treesitter { paths, view } => {
            format!("treesitter:{view:?}:{}", paths.join(","))
        }
        FetchSource::Lsp {
            server,
            query,
            paths,
        } => format!("lsp:{server}:{query:?}:{}", paths.join(",")),
        FetchSource::Mcp {
            server,
            resource,
            tool,
            ..
        } => format!(
            "mcp:{server}:{}",
            resource.as_deref().or(tool.as_deref()).unwrap_or("")
        ),
        FetchSource::Files { globs } => format!("files:{}", globs.join(",")),
    }
    .to_lowercase()
}

fn cut(text: &str, max_tokens: u64) -> String {
    let limit = usize::try_from(max_tokens).unwrap_or(usize::MAX);
    if tokens(text) <= limit {
        return text.to_owned();
    }
    let mut kept: Vec<&str> = Vec::new();
    for line in text.lines() {
        kept.push(line);
        if tokens(&kept.join("\n")) > limit {
            kept.pop();
            break;
        }
    }
    format!("{}\n[cut to {max_tokens} tokens]", kept.join("\n"))
}

fn expand(root: &Path, patterns: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        builder.add(Glob::new(pattern).map_err(|error| format!("{pattern}: {error}"))?);
    }
    let set = builder.build().map_err(|error| error.to_string())?;
    let mut found: Vec<PathBuf> = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker.filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        if let Ok(relative) = entry.path().strip_prefix(root)
            && set.is_match(relative)
        {
            found.push(relative.to_path_buf());
        }
    }
    found.sort();
    found.truncate(MAX_FILES);
    if found.is_empty() {
        return Err(format!(
            "nothing under the worktree matches {}",
            patterns.join(", ")
        ));
    }
    Ok(found)
}

fn read_text(root: &Path, relative: &Path) -> Result<String, String> {
    let path = root.join(relative);
    let meta = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("{}: {error}", relative.display()))?;
    if !meta.is_file() || meta.len() > MAX_FILE_BYTES {
        return Err(format!(
            "{} is not a regular file under {MAX_FILE_BYTES} bytes",
            relative.display()
        ));
    }
    std::fs::read_to_string(&path).map_err(|error| format!("{}: {error}", relative.display()))
}

pub async fn fetch(env: &ContextEnv, spec: &FetchSpec) -> Result<String, String> {
    let max = spec.max_tokens.unwrap_or(DEFAULT_SOURCE_TOKENS);
    let text = match &spec.source {
        FetchSource::Skill { name } => env
            .skills
            .get(name)
            .map(|skill| skill.body.clone())
            .ok_or_else(|| format!("skill `{name}` is not available"))?,
        FetchSource::Files { globs } => {
            let mut parts = Vec::new();
            for relative in expand(&env.root, globs)? {
                parts.push(format!(
                    "--- {}\n{}",
                    relative.display(),
                    read_text(&env.root, &relative)?
                ));
            }
            parts.join("\n")
        }
        FetchSource::Treesitter { paths, view } => {
            let mut parts = Vec::new();
            for relative in expand(&env.root, paths)? {
                let source = read_text(&env.root, &relative)?;
                let name = relative.display().to_string();
                let shape = outline(&name, &source)
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(|| source.lines().take(40).collect::<Vec<_>>().join("\n"));
                let shape = match view {
                    TreeView::Outline => shape,
                    TreeView::Symbols => shape
                        .lines()
                        .map(|line| line.trim().chars().take(120).collect::<String>())
                        .filter(|line| !line.is_empty())
                        .collect::<Vec<_>>()
                        .join("\n"),
                };
                parts.push(format!("--- {name}\n{shape}"));
            }
            parts.join("\n")
        }
        FetchSource::Mcp {
            server,
            resource,
            tool,
            args,
        } => {
            let provider = env
                .mcp
                .get(server)
                .ok_or_else(|| format!("mcp server `{server}` is not attached to this node"))?;
            match (resource, tool) {
                (Some(uri), None) => provider
                    .fetch_resource(uri)
                    .await
                    .map_err(|e| e.to_string())?,
                (None, Some(name)) => provider
                    .fetch_tool(name, args)
                    .await
                    .map_err(|e| e.to_string())?,
                _ => return Err("an mcp source needs exactly one of resource and tool".to_owned()),
            }
        }
        FetchSource::Lsp {
            server,
            query,
            paths,
        } => {
            let lsp = env
                .lsp
                .as_ref()
                .ok_or_else(|| format!("language server `{server}` is not configured"))?;
            let kind = match query {
                LspQuery::Diagnostics => LspKind::Diagnostics,
                LspQuery::Symbols => LspKind::Symbols,
                LspQuery::References => LspKind::References,
            };
            let files = expand(&env.root, paths)?;
            lsp.query(LspRequest {
                server,
                kind,
                root: &env.root,
                files: &files,
                confiner: &env.confiner,
            })
            .await
            .map_err(|error| error.to_string())?
        }
    };
    Ok(cut(&text, max))
}

pub struct LazyProvider {
    env: Arc<ContextEnv>,
    sources: Vec<(String, FetchSpec)>,
}

#[async_trait]
impl ToolProvider for LazyProvider {
    fn specs(&self) -> Vec<ToolSpec> {
        let labels: Vec<&str> = self
            .sources
            .iter()
            .map(|(label, _)| label.as_str())
            .collect();
        vec![ToolSpec {
            name: LAZY_TOOL.to_owned(),
            description: "Fetch one of the context sources prepared for this task.".to_owned(),
            input_schema: json!({
                "type": "object",
                "properties": {"source": {"type": "string", "enum": labels}},
                "required": ["source"],
            }),
        }]
    }

    fn replay(&self, _name: &str) -> Replay {
        Replay::Repeat
    }

    fn system_note(&self) -> Option<String> {
        let mut note = String::from("Context you can fetch with fetch_context when you need it:");
        for (label, _) in &self.sources {
            note.push_str(&format!("\n- {label}"));
        }
        Some(note)
    }

    async fn invoke(&self, _name: &str, input: &Value) -> Result<ProviderReply, ToolSetError> {
        let wanted = input
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some((_, spec)) = self.sources.iter().find(|(label, _)| label == wanted) else {
            return Ok(ProviderReply {
                text: format!("ERROR fetch_context: no source `{wanted}`"),
                is_error: true,
            });
        };
        Ok(match fetch(&self.env, spec).await {
            Ok(text) => ProviderReply {
                text,
                is_error: false,
            },
            Err(message) => ProviderReply {
                text: format!("ERROR fetch_context: {message}"),
                is_error: true,
            },
        })
    }
}

#[must_use]
pub fn lazy_provider(env: &Arc<ContextEnv>, node: &AgentNode) -> Option<LazyProvider> {
    let sources: Vec<(String, FetchSpec)> = node
        .context
        .fetch
        .iter()
        .filter(|spec| spec.mode == FetchMode::Lazy)
        .map(|spec| (label(spec), spec.clone()))
        .collect();
    (!sources.is_empty()).then(|| LazyProvider {
        env: Arc::clone(env),
        sources,
    })
}

fn summarize(value: &Value, max_tokens: u64) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    cut(&text, max_tokens)
}

async fn final_answer(shared: &Shared, upstream: &str) -> Option<String> {
    let session = format!("{}.{upstream}.0", shared.run_id);
    let events = shared.store.events(&session).await.ok()?;
    events
        .iter()
        .rev()
        .filter(|event| event.kind == EventKind::ModelResponded)
        .find_map(|event| {
            let blocks: Vec<ContentBlock> =
                serde_json::from_value(event.payload.get("content")?.clone()).ok()?;
            let text: String = blocks
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            (!text.is_empty()).then_some(text)
        })
}

pub async fn eager_text(
    shared: &Shared,
    id: &str,
    node: &AgentNode,
    inputs: &Values,
    env: &ContextEnv,
) -> Result<String, RunError> {
    let fail = |message: String| RunError::Context {
        node: id.to_owned(),
        message,
    };
    let mut blocks: Vec<(String, String)> = Vec::new();
    for inherit in &node.context.inherit {
        let value = inputs
            .get(&inherit.channel)
            .ok_or_else(|| fail(format!("channel `{}` has no value", inherit.channel)))?;
        let budget = inherit.max_tokens.unwrap_or(match inherit.mode {
            InheritMode::Summary => DEFAULT_SOURCE_TOKENS / 2,
            InheritMode::Full => DEFAULT_TOTAL_TOKENS,
        });
        blocks.push((
            format!("channel {} ({:?})", inherit.channel, inherit.mode).to_lowercase(),
            summarize(value, budget),
        ));
    }
    for upstream in &node.context.from_nodes {
        let answer = final_answer(shared, upstream)
            .await
            .ok_or_else(|| fail(format!("node `{upstream}` left no final answer to inherit")))?;
        blocks.push((
            format!("answer of node {upstream}"),
            cut(&answer, DEFAULT_SOURCE_TOKENS),
        ));
    }
    for spec in node
        .context
        .fetch
        .iter()
        .filter(|spec| spec.mode == FetchMode::Eager)
    {
        let name = label(spec);
        let text = fetch(env, spec)
            .await
            .map_err(|message| fail(format!("{name}: {message}")))?;
        blocks.push((name, text));
    }
    let total = node
        .context
        .max_context_tokens
        .unwrap_or(DEFAULT_TOTAL_TOKENS);
    let mut used = 0usize;
    let mut text = String::new();
    let mut dropped: Vec<String> = Vec::new();
    for (name, body) in blocks {
        let block = format!("<context source=\"{name}\">\n{body}\n</context>\n");
        let cost = tokens(&block);
        if used.saturating_add(cost) > usize::try_from(total).unwrap_or(usize::MAX) {
            dropped.push(name);
            continue;
        }
        used += cost;
        text.push_str(&block);
    }
    if !dropped.is_empty() {
        text.push_str(&format!(
            "[context budget of {total} tokens reached; left out: {}]\n",
            dropped.join(", ")
        ));
    }
    Ok(text)
}

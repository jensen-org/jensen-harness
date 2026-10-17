use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SPEC_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSpec {
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub channels: BTreeMap<String, ChannelSpec>,
    pub nodes: BTreeMap<String, NodeSpec>,
    #[serde(default)]
    pub edges: Vec<EdgeSpec>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_super_steps: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_wall_time_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Reducer {
    #[default]
    Overwrite,
    Append,
    SetUnion,
    Max,
    MergeMap,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChannelSpec {
    pub schema: Value,
    #[serde(default)]
    pub reducer: Reducer,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<Value>,
    #[serde(default)]
    pub verified: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolAccess {
    #[default]
    ReadOnly,
    Writer,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentNode {
    pub role: String,
    pub prompt: String,
    #[serde(default)]
    pub reads: Vec<String>,
    #[serde(default)]
    pub writes: Vec<String>,
    #[serde(default)]
    pub tools: ToolAccess,
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CommandNode {
    pub run: String,
    #[serde(default)]
    pub reads: Vec<String>,
    #[serde(default)]
    pub writes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default)]
    pub idempotent: bool,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifierNode {
    pub run: String,
    #[serde(default)]
    pub reads: Vec<String>,
    pub pass_when: String,
    pub passes: String,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouterNode {
    #[serde(default)]
    pub reads: Vec<String>,
    pub expr: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LoopNode {
    pub body: Vec<String>,
    pub until: String,
    pub max_iterations: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PromoteNode {
    pub from: String,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub protected: Vec<String>,
    pub max_changed_files: Option<usize>,
    pub max_added_bytes: Option<u64>,
    #[serde(default)]
    pub allow_binary: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishNode {
    pub from: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NodeSpec {
    Agent(AgentNode),
    Command(CommandNode),
    Verifier(VerifierNode),
    Router(RouterNode),
    Loop(LoopNode),
    Promote(PromoteNode),
    Publish(PublishNode),
}

impl NodeSpec {
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Agent(_) => "agent",
            Self::Command(_) => "command",
            Self::Verifier(_) => "verifier",
            Self::Router(_) => "router",
            Self::Loop(_) => "loop",
            Self::Promote(_) => "promote",
            Self::Publish(_) => "publish",
        }
    }

    #[must_use]
    pub fn reads(&self) -> &[String] {
        match self {
            Self::Agent(node) => &node.reads,
            Self::Command(node) => &node.reads,
            Self::Verifier(node) => &node.reads,
            Self::Router(node) => &node.reads,
            Self::Loop(_) | Self::Publish(_) => &[],
            Self::Promote(node) => &node.requires,
        }
    }
    #[must_use]
    pub fn writes(&self) -> Vec<String> {
        match self {
            Self::Agent(node) => node.writes.clone(),
            Self::Command(node) => node
                .writes
                .iter()
                .cloned()
                .chain(node.output.iter().cloned())
                .collect(),
            Self::Verifier(node) => vec![node.passes.clone()],
            Self::Router(_) | Self::Loop(_) => Vec::new(),
            Self::Promote(node) => node.output.iter().cloned().collect(),
            Self::Publish(node) => node.output.iter().cloned().collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EdgeSpec {
    pub from: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

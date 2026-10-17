//! The wire protocol jensen-harness speaks on every transport (stdio, a Unix
//! socket, and later a Windows named pipe): plain JSON-RPC 2.0, so a client
//! never has to guess the shape from one transport to the next.
//!
//! Every type here derives `JsonSchema`, so the protocol can be published as
//! a real schema a third party frontend can generate bindings from, instead
//! of reverse-engineering the wire format from this crate's source.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const JSONRPC_VERSION: &str = "2.0";

/// A JSON-RPC request id: either form is valid on the wire, and a server
/// must echo back whichever one the client sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RequestId {
    Number(i64),
    Text(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    #[must_use]
    pub fn new(id: RequestId, method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Notification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    #[must_use]
    pub fn new(method: impl Into<String>, params: Option<Value>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ResponseError {
    #[must_use]
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    #[must_use]
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(error_code::INTERNAL_ERROR, message)
    }
}

/// Standard JSON-RPC 2.0 error codes (<https://www.jsonrpc.org/specification#error_object>).
pub mod error_code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Response {
    pub jsonrpc: String,
    pub id: RequestId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

impl Response {
    #[must_use]
    pub fn ok(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn err(id: RequestId, error: ResponseError) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// What a raw JSON value on the wire turned out to be, decided the way the
/// JSON-RPC spec itself distinguishes them: a request and a notification
/// both carry `method`, a request also carries `id`; a response carries
/// `id` with neither `method` nor `params`.
#[derive(Debug, Clone)]
pub enum Frame {
    Request(Request),
    Notification(Notification),
    Response(Response),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("the frame is not a JSON object")]
    NotAnObject,
    #[error("a frame with \"method\" but no \"id\" must parse as a notification: {0}")]
    BadNotification(String),
    #[error("a frame with \"method\" and \"id\" must parse as a request: {0}")]
    BadRequest(String),
    #[error("a frame with \"id\" and no \"method\" must parse as a response: {0}")]
    BadResponse(String),
}

/// # Errors
/// Returns [`FrameError`] when the value is not an object, or has a shape
/// matching none of request, notification or response.
pub fn classify(raw: &Value) -> Result<Frame, FrameError> {
    let object = raw.as_object().ok_or(FrameError::NotAnObject)?;
    let has_method = object.contains_key("method");
    let has_id = object.contains_key("id");
    if has_method && has_id {
        let request: Request = serde_json::from_value(raw.clone())
            .map_err(|error| FrameError::BadRequest(error.to_string()))?;
        Ok(Frame::Request(request))
    } else if has_method {
        let notification: Notification = serde_json::from_value(raw.clone())
            .map_err(|error| FrameError::BadNotification(error.to_string()))?;
        Ok(Frame::Notification(notification))
    } else {
        let response: Response = serde_json::from_value(raw.clone())
            .map_err(|error| FrameError::BadResponse(error.to_string()))?;
        Ok(Frame::Response(response))
    }
}

/// Hard caps. A field left out is not capped by this request; the daemon's
/// configured caps, then its built in token cap, apply.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_cost_micro_usd: Option<u64>,
}

/// `run.new`: open a budget scope that sessions can join, so they share its
/// caps.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunNewParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RunNewResult {
    pub run_id: String,
}

/// `session.new`: start a session bound to one worktree. `model` is a
/// `provider_id/model_id` address and defaults to the daemon's own default;
/// `max_turns` caps the model calls one prompt may spend; `run_id` joins a
/// run opened with `run.new`; `budget` caps this session.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionNewParams {
    pub worktree: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_turns: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetParams>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionNewResult {
    pub session_id: String,
}

/// `session.prompt`: send one user turn and run it to completion.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionPromptParams {
    pub session_id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    Refusal,
    TurnLimit,
    BudgetExhausted,
    Error,
}

/// Tokens billed for one prompt, in separate buckets: `input_tokens`
/// excludes the cache buckets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

/// What the token saving layer kept out of the context during one prompt,
/// as estimates: tokens the raw tool output would have cost, and tokens
/// actually sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Savings {
    pub raw_tokens: u64,
    pub sent_tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionPromptResult {
    pub stop_reason: StopReason,
    pub model_calls: u32,
    pub cost_micro_usd: u64,
    pub usage: Usage,
    pub savings: Savings,
}

/// `session.resume`: continue a prompt that was cut short, by a provider
/// error or a restart, from where the journal says it stopped. The result is
/// the same as `session.prompt`'s.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionResumeParams {
    pub session_id: String,
}

/// `session.update`: a notification streamed while a prompt is running.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SessionUpdateParams {
    pub session_id: String,
    pub delta: String,
}

/// `workflow.start`: run the workflow file at `workflow` (relative paths are
/// taken from the worktree) over `worktree`, with `inputs` seeding its
/// channels. The call returns when the run ends.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStartParams {
    pub worktree: String,
    pub workflow: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<BudgetParams>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    BudgetExceeded,
    DeadlineExceeded,
}

/// What `workflow.start` and `workflow.resume` return. `outcomes` names how
/// each node that ran ended (`ok`, `failed`, `interrupted`, `stalled` or
/// `exhausted`); a node that was skipped has none.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowRunResult {
    pub run_id: String,
    pub status: WorkflowStatus,
    pub channels: BTreeMap<String, Value>,
    pub outcomes: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub super_steps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStatusParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowStatusResult {
    pub run_id: String,
    pub spec_hash: String,
    pub status: WorkflowStatus,
}

/// `workflow.resume`: continue a run from its last committed wave, from the
/// spec it was started with. Its result is `workflow.start`'s.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowResumeParams {
    pub run_id: String,
}

/// `workflow.cancel`: stop a run that is in flight in this daemon. Send it
/// on another connection than the one waiting for the run.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCancelParams {
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkflowCancelResult {
    pub cancelled: bool,
}

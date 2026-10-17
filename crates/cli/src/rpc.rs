//! The JSON-RPC handler shared by `serve` (a Unix socket) and `run` (stdio):
//! newline-delimited JSON in, newline-delimited JSON out, until the reader
//! closes. A client sees the same methods and the same framing regardless
//! of which transport it used to connect.

use std::sync::Arc;

use jensen_harness_core::agent::{
    AgentError, DEFAULT_MAX_TURNS, PromptOutcome, PromptStop, Recovered, Session, SessionConfig,
};
use jensen_harness_core::budget::{Caps, DEFAULT_SOFT_PERCENT};
use jensen_harness_core::store::{LedgerError, NewScope, ScopeKind, StoreError};
use jensen_harness_core::toolset::ToolSet;
use jensen_harness_protocol::{
    BudgetParams, Frame, Request, RequestId, Response, ResponseError, RunNewParams, RunNewResult,
    Savings, SessionNewParams, SessionNewResult, SessionPromptParams, SessionPromptResult,
    SessionResumeParams, StopReason, Usage, classify, error_code,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine::{Engine, LiveSession};

/// # Errors
/// Returns an I/O error if a response cannot be written; a request that
/// itself fails is reported back to the client as a JSON-RPC error, which
/// is not a return error here.
pub async fn serve_connection<R, W>(
    mut reader: R,
    mut writer: W,
    engine: Arc<Engine>,
) -> anyhow::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader.read_line(&mut line).await?;
        if read == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(raw) = serde_json::from_str::<Value>(trimmed) else {
            tracing::warn!(line = trimmed, "a line was not valid JSON and was dropped");
            continue;
        };
        let Ok(Frame::Request(request)) = classify(&raw) else {
            tracing::warn!(
                line = trimmed,
                "a line was not a JSON-RPC request and was dropped"
            );
            continue;
        };
        let response = dispatch(&engine, request).await;
        let encoded = serde_json::to_string(&response)?;
        writer.write_all(encoded.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
    }
}

async fn dispatch(engine: &Engine, request: Request) -> Response {
    match request.method.as_str() {
        "run.new" => run_new(engine, request.id, request.params).await,
        "session.new" => session_new(engine, request.id, request.params).await,
        "session.prompt" => session_prompt(engine, request.id, request.params).await,
        "session.resume" => session_resume(engine, request.id, request.params).await,
        other => Response::err(
            request.id,
            ResponseError::new(
                error_code::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            ),
        ),
    }
}

fn invalid_params(id: RequestId, message: impl Into<String>) -> Response {
    Response::err(id, ResponseError::new(error_code::INVALID_PARAMS, message))
}

fn internal(id: RequestId, message: impl Into<String>) -> Response {
    Response::err(id, ResponseError::internal(message))
}

fn parse_params<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Option<Value>,
) -> Result<T, String> {
    let params = params.ok_or_else(|| format!("{method} needs params"))?;
    serde_json::from_value(params).map_err(|error| error.to_string())
}

fn caps_of(budget: Option<BudgetParams>) -> Option<Caps> {
    budget.map(|budget| Caps {
        max_tokens: budget.max_tokens,
        max_cost_micro_usd: budget.max_cost_micro_usd,
    })
}

/// What the client did wrong versus what the harness could not do.
fn agent_error(id: RequestId, error: &AgentError) -> Response {
    match error {
        AgentError::PromptOpen | AgentError::NothingToResume | AgentError::UnknownSession(_) => {
            invalid_params(id, error.to_string())
        }
        AgentError::Store(StoreError::Ledger(
            LedgerError::UnknownScope(_) | LedgerError::ScopeExists(_),
        )) => invalid_params(id, error.to_string()),
        AgentError::Provider(_) => internal(id, format!("provider: {error}")),
        AgentError::Tool(_) => internal(id, format!("tool: {error}")),
        AgentError::Store(_) | AgentError::Budget(_) | AgentError::Journal(_) => {
            internal(id, format!("state: {error}"))
        }
    }
}

async fn run_new(engine: &Engine, id: RequestId, params: Option<Value>) -> Response {
    let params: RunNewParams = if params.is_none() {
        RunNewParams::default()
    } else {
        match parse_params("run.new", params) {
            Ok(params) => params,
            Err(message) => return invalid_params(id, message),
        }
    };
    let caps = match caps_of(params.budget).unwrap_or_default().validated() {
        Ok(caps) => caps,
        Err(error) => return invalid_params(id, error.to_string()),
    };
    let run_id = uuid::Uuid::new_v4().to_string();
    let scope = NewScope {
        id: run_id.clone(),
        parent_id: None,
        kind: ScopeKind::Run,
        caps,
        soft_percent: engine.budget.soft_percent.unwrap_or(DEFAULT_SOFT_PERCENT),
    };
    match engine.store.open_scope(scope).await {
        Ok(()) => Response::ok(id, json!(RunNewResult { run_id })),
        Err(error) => internal(id, format!("state: {error}")),
    }
}

async fn session_new(engine: &Engine, id: RequestId, params: Option<Value>) -> Response {
    let params: SessionNewParams = match parse_params("session.new", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    if params.max_turns == Some(0) {
        return invalid_params(id, "max_turns must be at least 1");
    }
    let resolved = match engine.models.resolve(params.model.as_deref()) {
        Ok(resolved) => resolved,
        Err(error) => return invalid_params(id, error.to_string()),
    };
    let budget = match engine
        .budget
        .resolve(caps_of(params.budget), &resolved.address)
    {
        Ok(budget) => budget,
        Err(error) => return invalid_params(id, error.to_string()),
    };
    let tools = match ToolSet::for_worktree(std::path::Path::new(&params.worktree)) {
        Ok(tools) => tools,
        Err(error) => {
            return invalid_params(id, format!("worktree {}: {error}", params.worktree));
        }
    };
    let session_id = uuid::Uuid::new_v4().to_string();
    let config = SessionConfig {
        session_id: session_id.clone(),
        run_id: params.run_id,
        model_address: resolved.address,
        max_turns: params.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
        budget,
        price_version: engine.budget.pricing.version().to_string(),
    };
    let session = match Session::create(engine.store.clone(), config, tools).await {
        Ok(session) => session,
        Err(error) => return agent_error(id, &error),
    };
    engine.sessions.lock().await.insert(
        session_id.clone(),
        Arc::new(tokio::sync::Mutex::new(LiveSession {
            provider: resolved.provider,
            session,
        })),
    );
    Response::ok(id, json!(SessionNewResult { session_id }))
}

fn stop_reason(stop: PromptStop) -> StopReason {
    match stop {
        PromptStop::EndTurn => StopReason::EndTurn,
        PromptStop::StopSequence => StopReason::StopSequence,
        PromptStop::MaxTokens => StopReason::MaxTokens,
        PromptStop::Refusal => StopReason::Refusal,
        PromptStop::TurnLimit => StopReason::TurnLimit,
        PromptStop::BudgetExhausted => StopReason::BudgetExhausted,
    }
}

fn prompt_result(
    outcome: &PromptOutcome,
) -> Result<SessionPromptResult, std::num::TryFromIntError> {
    Ok(SessionPromptResult {
        stop_reason: stop_reason(outcome.stop),
        model_calls: outcome.model_calls,
        cost_micro_usd: outcome.cost_micro_usd,
        usage: Usage {
            input_tokens: outcome.usage.input_tokens,
            output_tokens: outcome.usage.output_tokens,
            cache_read_tokens: outcome.usage.cache_read_tokens,
            cache_write_tokens: outcome.usage.cache_write_tokens,
        },
        savings: Savings {
            raw_tokens: u64::try_from(outcome.savings.raw_tokens)?,
            sent_tokens: u64::try_from(outcome.savings.sent_tokens)?,
        },
    })
}

/// The live session for an id. One the daemon does not hold is rebuilt from
/// the journal, so sessions survive a restart.
async fn live_session(
    engine: &Engine,
    id: &RequestId,
    session_id: &str,
) -> Result<Arc<tokio::sync::Mutex<LiveSession>>, Response> {
    let mut sessions = engine.sessions.lock().await;
    if let Some(live) = sessions.get(session_id) {
        return Ok(Arc::clone(live));
    }
    let recovered = Recovered::load(&engine.store, session_id)
        .await
        .map_err(|error| agent_error(id.clone(), &error))?;
    let resolved = engine
        .models
        .resolve(Some(&recovered.config.model_address))
        .map_err(|error| internal(id.clone(), format!("model: {error}")))?;
    let tools = ToolSet::for_worktree(&recovered.worktree).map_err(|error| {
        internal(
            id.clone(),
            format!("worktree {}: {error}", recovered.worktree.display()),
        )
    })?;
    let session = Session::restore(engine.store.clone(), recovered, tools)
        .await
        .map_err(|error| agent_error(id.clone(), &error))?;
    let live = Arc::new(tokio::sync::Mutex::new(LiveSession {
        provider: resolved.provider,
        session,
    }));
    sessions.insert(session_id.to_string(), Arc::clone(&live));
    Ok(live)
}

fn outcome_response(id: RequestId, result: Result<PromptOutcome, AgentError>) -> Response {
    match result {
        Ok(outcome) => match prompt_result(&outcome) {
            Ok(result) => Response::ok(id, json!(result)),
            Err(error) => internal(id, error.to_string()),
        },
        Err(error) => agent_error(id, &error),
    }
}

async fn session_prompt(engine: &Engine, id: RequestId, params: Option<Value>) -> Response {
    let params: SessionPromptParams = match parse_params("session.prompt", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    let live = match live_session(engine, &id, &params.session_id).await {
        Ok(live) => live,
        Err(response) => return response,
    };
    let mut live = live.lock().await;
    let LiveSession { provider, session } = &mut *live;
    let result = session.prompt(provider.as_ref(), &params.message).await;
    outcome_response(id, result)
}

async fn session_resume(engine: &Engine, id: RequestId, params: Option<Value>) -> Response {
    let params: SessionResumeParams = match parse_params("session.resume", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    let live = match live_session(engine, &id, &params.session_id).await {
        Ok(live) => live,
        Err(response) => return response,
    };
    let mut live = live.lock().await;
    let LiveSession { provider, session } = &mut *live;
    let result = session.resume(provider.as_ref()).await;
    outcome_response(id, result)
}

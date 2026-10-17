//! The JSON-RPC handler shared by `serve` (a Unix socket) and `run` (stdio):
//! newline-delimited JSON in, newline-delimited JSON out, until the reader
//! closes. A client sees the same methods and the same framing regardless
//! of which transport it used to connect.

use std::sync::Arc;

use jensen_harness_core::providers::StopReason as EngineStopReason;
use jensen_harness_protocol::{
    Frame, Request, RequestId, Response, ResponseError, SessionNewResult, SessionPromptParams,
    SessionPromptResult, StopReason, classify, error_code,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

use crate::engine::Engine;

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
        "session.new" => session_new(engine, request.id).await,
        "session.prompt" => session_prompt(engine, request.id, request.params).await,
        other => Response::err(
            request.id,
            ResponseError::new(
                error_code::METHOD_NOT_FOUND,
                format!("unknown method: {other}"),
            ),
        ),
    }
}

async fn session_new(engine: &Engine, id: RequestId) -> Response {
    let session_id = uuid::Uuid::new_v4().to_string();
    engine.sessions.lock().await.insert(
        session_id.clone(),
        jensen_harness_core::agent::Session::new(),
    );
    let result = SessionNewResult { session_id };
    Response::ok(id, json!(result))
}

async fn session_prompt(engine: &Engine, id: RequestId, params: Option<Value>) -> Response {
    let Some(params) = params else {
        return Response::err(
            id,
            ResponseError::new(error_code::INVALID_PARAMS, "session.prompt needs params"),
        );
    };
    let params: SessionPromptParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return Response::err(
                id,
                ResponseError::new(error_code::INVALID_PARAMS, error.to_string()),
            );
        }
    };
    let mut sessions = engine.sessions.lock().await;
    let Some(session) = sessions.get_mut(&params.session_id) else {
        return Response::err(
            id,
            ResponseError::new(
                error_code::INVALID_PARAMS,
                format!("no such session: {}", params.session_id),
            ),
        );
    };
    match session
        .prompt(engine.provider.as_ref(), &params.message)
        .await
    {
        Ok(stop_reason) => {
            let stop_reason = match stop_reason {
                EngineStopReason::EndTurn => StopReason::EndTurn,
                EngineStopReason::MaxTokens => StopReason::MaxTokens,
            };
            Response::ok(id, json!(SessionPromptResult { stop_reason }))
        }
        Err(error) => Response::err(id, ResponseError::internal(error.to_string())),
    }
}

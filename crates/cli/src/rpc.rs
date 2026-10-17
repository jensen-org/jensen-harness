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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::serve_connection;
    use crate::engine::Engine;
    use async_trait::async_trait;
    use jensen_harness_core::providers::{
        CompletionRequest, CompletionResponse, Provider, ProviderError,
        StopReason as EngineStopReason,
    };
    use jensen_harness_protocol::{RequestId, SessionNewResult, SessionPromptResult, StopReason};
    use std::sync::Arc;
    use tokio::io::BufReader;

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
                stop_reason: EngineStopReason::EndTurn,
            })
        }
    }

    #[tokio::test]
    async fn a_new_session_then_a_prompt_round_trips() {
        let engine = Arc::new(Engine::new(Box::new(Echo)));
        let input = format!(
            "{}\n",
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "session.new", "params": {}})
        );
        let reader = BufReader::new(input.as_bytes());
        let mut output = Vec::new();
        serve_connection(reader, &mut output, Arc::clone(&engine))
            .await
            .unwrap();
        let first_line = String::from_utf8(output).unwrap();
        let response: jensen_harness_protocol::Response =
            serde_json::from_str(first_line.trim()).unwrap();
        assert_eq!(response.id, RequestId::Number(1));
        let result: SessionNewResult = serde_json::from_value(response.result.unwrap()).unwrap();

        let prompt_input = format!(
            "{}\n",
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "session.prompt",
                "params": {"session_id": result.session_id, "message": "hello"},
            })
        );
        let reader = BufReader::new(prompt_input.as_bytes());
        let mut output = Vec::new();
        serve_connection(reader, &mut output, engine).await.unwrap();
        let second_line = String::from_utf8(output).unwrap();
        let response: jensen_harness_protocol::Response =
            serde_json::from_str(second_line.trim()).unwrap();
        let result: SessionPromptResult = serde_json::from_value(response.result.unwrap()).unwrap();
        assert_eq!(result.stop_reason, StopReason::EndTurn);
    }

    #[tokio::test]
    async fn an_unknown_method_is_reported_as_a_jsonrpc_error_not_a_dropped_connection() {
        let engine = Arc::new(Engine::new(Box::new(Echo)));
        let input = format!(
            "{}\n",
            serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "nope", "params": {}})
        );
        let reader = BufReader::new(input.as_bytes());
        let mut output = Vec::new();
        serve_connection(reader, &mut output, engine).await.unwrap();
        let response: jensen_harness_protocol::Response =
            serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
        assert!(response.error.is_some());
    }
}

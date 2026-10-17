use std::sync::Arc;
use std::time::Duration;

use crate::common::{Echo, Scripted, calls_tool, path_input, says};
use async_trait::async_trait;
use jensen_harness::engine::Engine;
use jensen_harness::rpc::serve_connection;
use jensen_harness_core::providers::{
    CompletionRequest, CompletionResponse, Provider, ProviderError,
};
use jensen_harness_protocol::{
    RequestId, Response, SessionNewResult, SessionPromptResult, StopReason, error_code,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

async fn call(engine: &Arc<Engine>, request: Value) -> Response {
    let input = format!("{request}\n");
    let reader = BufReader::new(input.as_bytes());
    let mut output = Vec::new();
    serve_connection(reader, &mut output, Arc::clone(engine))
        .await
        .unwrap();
    serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap()
}

fn new_session(id: i64, worktree: &std::path::Path) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session.new",
           "params": {"worktree": worktree.to_str().unwrap()}})
}

fn prompt(id: i64, session_id: &str, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session.prompt",
           "params": {"session_id": session_id, "message": message}})
}

async fn open_session(engine: &Arc<Engine>, worktree: &std::path::Path) -> String {
    let response = call(engine, new_session(1, worktree)).await;
    let result: SessionNewResult = serde_json::from_value(response.result.unwrap()).unwrap();
    result.session_id
}

#[tokio::test]
async fn a_new_session_then_a_prompt_round_trips_with_usage_and_savings() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::with_fixed_model(Arc::new(Echo), "echo-model").unwrap());
    let session_id = open_session(&engine, dir.path()).await;

    let response = call(&engine, prompt(2, &session_id, "hello")).await;

    assert_eq!(response.id, RequestId::Number(2));
    let result: SessionPromptResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(result.stop_reason, StopReason::EndTurn);
    assert_eq!(result.model_calls, 1);
    assert_eq!(result.savings.raw_tokens, 0);
}

#[tokio::test]
async fn a_run_that_uses_tools_reports_what_it_saved_and_changes_the_worktree() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let provider = Arc::new(Scripted::of(vec![
        calls_tool("t", "read", path_input("a.txt")),
        says("done"),
    ]));
    let engine = Arc::new(Engine::with_fixed_model(provider, "m").unwrap());
    let session_id = open_session(&engine, dir.path()).await;

    let response = call(&engine, prompt(2, &session_id, "look")).await;

    let result: SessionPromptResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        (result.stop_reason, result.model_calls),
        (StopReason::EndTurn, 2)
    );
    assert_eq!(result.usage.input_tokens, 20);
    assert_eq!(result.usage.cache_read_tokens, 200);
    assert!(result.savings.raw_tokens > 0);
}

#[tokio::test]
async fn a_capped_run_reports_turn_limit() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "x").unwrap();
    let provider = Arc::new(Scripted::repeating(calls_tool(
        "t",
        "read",
        path_input("a.txt"),
    )));
    let engine = Arc::new(Engine::with_fixed_model(provider, "m").unwrap());
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "session.new",
        "params": {"worktree": dir.path().to_str().unwrap(), "max_turns": 2}});
    let response = call(&engine, request).await;
    let session: SessionNewResult = serde_json::from_value(response.result.unwrap()).unwrap();

    let response = call(&engine, prompt(2, &session.session_id, "loop")).await;

    let result: SessionPromptResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        (result.stop_reason, result.model_calls),
        (StopReason::TurnLimit, 2)
    );
}

#[tokio::test]
async fn bad_session_params_are_invalid_params_not_a_dropped_connection() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::with_fixed_model(Arc::new(Echo), "m").unwrap());
    let missing = dir.path().join("does-not-exist");
    let a_file = dir.path().join("file.txt");
    std::fs::write(&a_file, "x").unwrap();
    let requests = [
        new_session(1, &missing),
        new_session(2, &a_file),
        json!({"jsonrpc": "2.0", "id": 3, "method": "session.new", "params": {}}),
        json!({"jsonrpc": "2.0", "id": 4, "method": "session.new"}),
        json!({"jsonrpc": "2.0", "id": 5, "method": "session.new",
               "params": {"worktree": dir.path().to_str().unwrap(), "max_turns": 0}}),
        prompt(6, "no-such-session", "hi"),
    ];
    for request in requests {
        let response = call(&engine, request.clone()).await;
        let error = response
            .error
            .unwrap_or_else(|| panic!("no error for {request}"));
        assert_eq!(
            error.code,
            error_code::INVALID_PARAMS,
            "{request}: {}",
            error.message
        );
    }
}

#[tokio::test]
async fn a_provider_failure_is_an_internal_error_and_the_session_survives() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(Scripted::new(vec![
        Err(ProviderError::Upstream {
            provider: "scripted".into(),
            message: "boom".into(),
        }),
        Ok(says("recovered")),
    ]));
    let engine = Arc::new(Engine::with_fixed_model(provider, "m").unwrap());
    let session_id = open_session(&engine, dir.path()).await;

    let failed = call(&engine, prompt(2, &session_id, "one")).await;
    assert_eq!(failed.error.unwrap().code, error_code::INTERNAL_ERROR);

    let recovered = call(&engine, prompt(3, &session_id, "two")).await;
    assert!(recovered.error.is_none());
}

#[tokio::test]
async fn an_unknown_method_is_reported_as_a_jsonrpc_error_not_a_dropped_connection() {
    let engine = Arc::new(Engine::with_fixed_model(Arc::new(Echo), "echo-model").unwrap());
    let response = call(
        &engine,
        json!({"jsonrpc": "2.0", "id": 1, "method": "nope", "params": {}}),
    )
    .await;
    assert!(response.error.is_some());
}

#[derive(Debug)]
struct Gated {
    release: tokio::sync::Notify,
}

#[async_trait]
impl Provider for Gated {
    fn id(&self) -> &str {
        "gated"
    }

    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.release.notified().await;
        Ok(says("released"))
    }
}

#[tokio::test]
async fn a_long_run_on_one_session_does_not_block_another_session_or_a_new_one() {
    let dir = tempfile::tempdir().unwrap();
    let gated = Arc::new(Gated {
        release: tokio::sync::Notify::new(),
    });
    let engine =
        Arc::new(Engine::with_fixed_model(Arc::clone(&gated) as Arc<dyn Provider>, "m").unwrap());
    let busy = open_session(&engine, dir.path()).await;

    let (client, server) = tokio::io::duplex(8192);
    let (server_read, server_write) = tokio::io::split(server);
    let serving = tokio::spawn(serve_connection(
        BufReader::new(server_read),
        server_write,
        Arc::clone(&engine),
    ));
    let (client_read, mut client_write) = tokio::io::split(client);
    client_write
        .write_all(format!("{}\n", prompt(2, &busy, "wait")).as_bytes())
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let other = tokio::time::timeout(Duration::from_secs(5), async {
        let second = open_session(&engine, dir.path()).await;
        let engine_for_other = Arc::clone(&engine);
        let response = tokio::time::timeout(
            Duration::from_millis(300),
            call(&engine_for_other, prompt(3, &second, "also wait")),
        )
        .await;
        (second, response.is_err())
    })
    .await
    .expect("session.new must not wait for another session's run");
    assert!(
        other.1,
        "the second session also waits on the gate, not on the first"
    );

    gated.release.notify_waiters();
    let mut line = String::new();
    BufReader::new(client_read)
        .read_line(&mut line)
        .await
        .unwrap();
    let response: Response = serde_json::from_str(line.trim()).unwrap();
    assert!(response.error.is_none(), "{response:?}");
    drop(client_write);
    serving.await.unwrap().unwrap();
}

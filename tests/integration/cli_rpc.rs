use crate::common::Echo;
use jensen_harness::engine::Engine;
use jensen_harness::rpc::serve_connection;
use jensen_harness_protocol::{RequestId, SessionNewResult, SessionPromptResult, StopReason};
use std::sync::Arc;
use tokio::io::BufReader;

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

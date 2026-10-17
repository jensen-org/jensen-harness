use jensen_harness_core::providers::genai::{GenaiProvider, parse_adapter};
use jensen_harness_core::providers::{
    CompletionRequest, ContentBlock, Provider, ProviderError, Role, StopReason, ToolSpec, Turn,
    Usage,
};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn provider(server: &MockServer) -> GenaiProvider {
    GenaiProvider::new(
        "gateway",
        parse_adapter("openai_compatible").unwrap(),
        format!("{}/v1", server.uri()),
        "test-key",
    )
}

fn request(turns: Vec<Turn>) -> CompletionRequest {
    CompletionRequest {
        model: "some-vendor/some-model".to_string(),
        system: "be terse".to_string(),
        turns,
        tools: vec![ToolSpec {
            name: "read".to_string(),
            description: "read a file".to_string(),
            input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        }],
    }
}

async fn mount(server: &MockServer, finish_reason: &str, message: Value, usage: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "x",
            "object": "chat.completion",
            "model": "some-vendor/some-model",
            "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
            "usage": usage,
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_tool_call_from_an_openai_compatible_endpoint_becomes_a_tool_use() {
    let server = MockServer::start().await;
    mount(
        &server,
        "tool_calls",
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "read", "arguments": "{\"path\":\"a.rs\"}"},
            }],
        }),
        json!({"prompt_tokens": 120, "completion_tokens": 5, "total_tokens": 125,
               "prompt_tokens_details": {"cached_tokens": 100}}),
    )
    .await;

    let response = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")]))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.content,
        vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "read".to_string(),
            input: json!({"path": "a.rs"}),
        }]
    );
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 20,
            output_tokens: 5,
            cache_read_tokens: 100,
            cache_write_tokens: 0,
        }
    );
}

#[tokio::test]
async fn tool_results_go_back_as_tool_role_messages_and_errors_are_marked() {
    let server = MockServer::start().await;
    mount(
        &server,
        "stop",
        json!({"role": "assistant", "content": "done"}),
        json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}),
    )
    .await;
    let turns = vec![
        Turn::text(Role::User, "hi"),
        Turn {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: json!({"path": "a.rs"}),
            }],
        },
        Turn {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "no such file".to_string(),
                is_error: true,
            }],
        },
    ];

    let response = provider(&server).complete(request(turns)).await.unwrap();

    assert_eq!(response.stop_reason, StopReason::EndTurn);
    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    let tool = messages
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(tool["tool_call_id"], "call_1");
    assert_eq!(tool["content"], "ERROR: no such file");
    assert_eq!(body["tools"][0]["function"]["name"], "read");
}

#[tokio::test]
async fn an_unknown_finish_reason_is_an_error_not_a_default() {
    let server = MockServer::start().await;
    mount(
        &server,
        "something_new",
        json!({"role": "assistant", "content": "x"}),
        json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}),
    )
    .await;

    let error = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")]))
        .await
        .unwrap_err();

    assert!(matches!(error, ProviderError::Malformed(_)));
}

#[tokio::test]
async fn content_filter_maps_to_refusal() {
    let server = MockServer::start().await;
    mount(
        &server,
        "content_filter",
        json!({"role": "assistant", "content": ""}),
        json!({"prompt_tokens": 1, "completion_tokens": 0, "total_tokens": 1}),
    )
    .await;

    let response = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")]))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, StopReason::Refusal);
}

#[tokio::test]
async fn an_http_error_surfaces_as_an_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(401).set_body_string("bad key"))
        .mount(&server)
        .await;

    let error = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")]))
        .await
        .unwrap_err();

    assert!(matches!(error, ProviderError::Upstream { .. }));
}

#[test]
fn every_documented_adapter_name_parses_and_a_made_up_one_does_not() {
    for name in [
        "openai",
        "openai_compatible",
        "gemini",
        "deepseek",
        "groq",
        "xai",
        "ollama",
        "openrouter",
    ] {
        assert!(parse_adapter(name).is_some(), "{name}");
    }
    assert!(parse_adapter("not-a-vendor").is_none());
}

#[tokio::test]
async fn an_error_result_that_already_says_error_is_not_prefixed_twice() {
    let server = MockServer::start().await;
    mount(
        &server,
        "stop",
        json!({"role": "assistant", "content": "done"}),
        json!({"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}),
    )
    .await;
    let turns = vec![
        Turn::text(Role::User, "hi"),
        Turn {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "read".to_string(),
                input: json!({"path": "a.rs"}),
            }],
        },
        Turn {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "ERROR read: a.rs: no such file".to_string(),
                is_error: true,
            }],
        },
    ];

    provider(&server).complete(request(turns)).await.unwrap();

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    let tool = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(tool["content"], "ERROR read: a.rs: no such file");
}

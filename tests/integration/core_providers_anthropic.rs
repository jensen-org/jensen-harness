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
        "anthropic",
        parse_adapter("anthropic").unwrap(),
        format!("{}/v1", server.uri()),
        "test-key",
    )
}

fn request(turns: Vec<Turn>, tools: Vec<ToolSpec>) -> CompletionRequest {
    CompletionRequest {
        model: "claude-sonnet-5-5".to_string(),
        system: "be terse".to_string(),
        turns,
        tools,
    }
}

fn read_tool() -> ToolSpec {
    ToolSpec {
        name: "read".to_string(),
        description: "read a file".to_string(),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
    }
}

async fn mount(server: &MockServer, body: Value) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

async fn last_request_body(server: &MockServer) -> Value {
    let requests = server.received_requests().await.unwrap();
    serde_json::from_slice(&requests.last().unwrap().body).unwrap()
}

#[tokio::test]
async fn parses_a_text_response_and_its_cache_usage() {
    let server = MockServer::start().await;
    mount(
        &server,
        json!({
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 100,
                "cache_creation_input_tokens": 7,
            },
        }),
    )
    .await;

    let response = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")], vec![]))
        .await
        .unwrap();

    assert_eq!(
        response.content,
        vec![ContentBlock::Text("hello".to_string())]
    );
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 10,
            output_tokens: 3,
            cache_read_tokens: 100,
            cache_write_tokens: 7,
        }
    );
}

#[tokio::test]
async fn a_reply_with_only_a_tool_call_parses_with_the_tool_use_stop_reason() {
    let server = MockServer::start().await;
    mount(
        &server,
        json!({
            "content": [{"type": "tool_use", "id": "toolu_1", "name": "read", "input": {"path": "a.rs"}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }),
    )
    .await;

    let response = provider(&server)
        .complete(request(
            vec![Turn::text(Role::User, "hi")],
            vec![read_tool()],
        ))
        .await
        .unwrap();

    assert_eq!(response.stop_reason, StopReason::ToolUse);
    assert_eq!(
        response.content,
        vec![ContentBlock::ToolUse {
            id: "toolu_1".to_string(),
            name: "read".to_string(),
            input: json!({"path": "a.rs"}),
        }]
    );
}

#[tokio::test]
async fn the_system_prompt_and_the_newest_turn_carry_their_cache_ttls() {
    let server = MockServer::start().await;
    mount(
        &server,
        json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }),
    )
    .await;
    let turns = vec![
        Turn::text(Role::User, "hi"),
        Turn {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "toolu_1".to_string(),
                name: "read".to_string(),
                input: json!({"path": "a.rs"}),
            }],
        },
        Turn {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "toolu_1".to_string(),
                content: "fn main() {}".to_string(),
                is_error: false,
            }],
        },
    ];

    provider(&server)
        .complete(request(turns, vec![read_tool()]))
        .await
        .unwrap();

    let body = last_request_body(&server).await;
    assert_eq!(
        body["system"][0]["cache_control"]["ttl"], "1h",
        "system: {}",
        body["system"]
    );
    let messages = body["messages"].as_array().unwrap();
    let tail = messages.last().unwrap()["content"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert!(tail.get("cache_control").is_some(), "newest turn: {tail}");
}

#[tokio::test]
async fn tool_results_for_one_reply_share_one_user_message() {
    let server = MockServer::start().await;
    mount(
        &server,
        json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }),
    )
    .await;
    let turns = vec![
        Turn::text(Role::User, "hi"),
        Turn {
            role: Role::Assistant,
            content: vec![
                ContentBlock::ToolUse {
                    id: "a".to_string(),
                    name: "read".to_string(),
                    input: json!({"path": "a"}),
                },
                ContentBlock::ToolUse {
                    id: "b".to_string(),
                    name: "read".to_string(),
                    input: json!({"path": "b"}),
                },
            ],
        },
        Turn {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "a".to_string(),
                    content: "fine".to_string(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "b".to_string(),
                    content: "no such file".to_string(),
                    is_error: true,
                },
            ],
        },
    ];

    provider(&server)
        .complete(request(turns, vec![read_tool()]))
        .await
        .unwrap();

    let body = last_request_body(&server).await;
    let messages = body["messages"].as_array().unwrap();
    let results = messages.last().unwrap();
    assert_eq!(results["role"], "user");
    let kinds: Vec<&str> = results["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|block| block["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["tool_result", "tool_result"]);
}

#[tokio::test]
async fn an_unknown_stop_reason_is_an_error_not_a_default() {
    let server = MockServer::start().await;
    mount(
        &server,
        json!({
            "content": [{"type": "text", "text": "x"}],
            "stop_reason": "something_new",
            "usage": {"input_tokens": 1, "output_tokens": 1},
        }),
    )
    .await;

    let error = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")], vec![]))
        .await
        .unwrap_err();

    assert!(matches!(error, ProviderError::Malformed(_)));
}

#[tokio::test]
async fn refusal_and_max_tokens_map_to_their_own_stop_reasons() {
    for (wire, expected) in [
        ("max_tokens", StopReason::MaxTokens),
        ("stop_sequence", StopReason::StopSequence),
        ("refusal", StopReason::Refusal),
        ("pause_turn", StopReason::PauseTurn),
    ] {
        let server = MockServer::start().await;
        mount(
            &server,
            json!({
                "content": [{"type": "text", "text": "x"}],
                "stop_reason": wire,
                "usage": {"input_tokens": 1, "output_tokens": 1},
            }),
        )
        .await;
        let response = provider(&server)
            .complete(request(vec![Turn::text(Role::User, "hi")], vec![]))
            .await
            .unwrap();
        assert_eq!(response.stop_reason, expected, "wire reason {wire}");
    }
}

#[tokio::test]
async fn a_non_success_status_reports_an_upstream_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .mount(&server)
        .await;

    let error = provider(&server)
        .complete(request(vec![Turn::text(Role::User, "hi")], vec![]))
        .await
        .unwrap_err();

    assert!(matches!(error, ProviderError::Upstream { .. }));
}

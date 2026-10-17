use jensen_harness_core::providers::anthropic::AnthropicProvider;
use jensen_harness_core::providers::{
    CompletionRequest, Provider, ProviderError, Role, StopReason, Turn, require_api_key,
};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[test]
fn require_api_key_is_a_loud_error_not_a_silent_default_when_unset() {
    let unused_env_var = "JENSEN_HARNESS_TEST_UNSET_KEY_D9B4F2";
    let error = require_api_key("anthropic", unused_env_var).unwrap_err();
    assert!(matches!(error, ProviderError::MissingApiKey { .. }));
}

#[tokio::test]
async fn sends_the_messages_api_shape_and_parses_a_text_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": [{"type": "text", "text": "hello"}],
            "stop_reason": "end_turn",
        })))
        .mount(&server)
        .await;

    let provider =
        AnthropicProvider::new("anthropic", server.uri(), "test-key", "claude-sonnet-5-5");

    let response = provider
        .complete(CompletionRequest {
            system: "be terse".to_string(),
            turns: vec![Turn {
                role: Role::User,
                text: "hi".to_string(),
            }],
        })
        .await
        .unwrap();

    assert_eq!(response.text, "hello");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
}

#[tokio::test]
#[ignore = "makes a real network call; run with ANTHROPIC_API_KEY set and --ignored"]
async fn a_real_anthropic_call_round_trips() {
    let api_key = require_api_key("anthropic", "ANTHROPIC_API_KEY").unwrap();
    let provider = AnthropicProvider::new(
        "anthropic",
        "https://api.anthropic.com",
        api_key,
        "claude-haiku-5-5",
    );
    let response = provider
        .complete(CompletionRequest {
            system: "reply with exactly one word".to_string(),
            turns: vec![Turn {
                role: Role::User,
                text: "say hi".to_string(),
            }],
        })
        .await
        .unwrap();
    assert!(!response.text.is_empty());
}

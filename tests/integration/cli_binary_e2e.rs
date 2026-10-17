//! The real binary, end to end, with no key and no network: a local server
//! plays an OpenAI compatible provider. Covers the provider config, genai,
//! the daemon, the loop and the file tools in one run.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use crate::common::binary_path;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn completion(finish_reason: &str, message: Value) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "x",
        "object": "chat.completion",
        "model": "some-model",
        "choices": [{"index": 0, "message": message, "finish_reason": finish_reason}],
        "usage": {"prompt_tokens": 50, "completion_tokens": 7, "total_tokens": 57},
    }))
}

fn write_config(dir: &std::path::Path, base_url: &str) -> std::path::PathBuf {
    let config = dir.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "default_model = \"fake/some-model\"\n\n[providers.fake]\ntransport = \"openai_compatible\"\nbase_url = \"{base_url}/v1\"\napi_key_env = \"JENSEN_HARNESS_E2E_KEY\"\n"
        ),
    )
    .unwrap();
    config
}

#[tokio::test(flavor = "multi_thread")]
async fn the_binary_answers_a_prompt_by_reading_a_file_through_a_configured_provider() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(completion(
            "tool_calls",
            json!({
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "read", "arguments": "{\"path\":\"notes.txt\"}"},
                }],
            }),
        ))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(completion(
            "stop",
            json!({"role": "assistant", "content": "4417"}),
        ))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(worktree.path().join("notes.txt"), "alpha\ntoken: 4417\n").unwrap();
    let config = write_config(dir.path(), &server.uri());
    let state = dir.path().join("state.sqlite");
    let worktree_path = worktree.path().to_str().unwrap().to_string();

    let lines = tokio::task::spawn_blocking(move || {
        let mut child = Command::new(binary_path())
            .arg("--config")
            .arg(&config)
            .arg("--state")
            .arg(&state)
            .arg("run")
            .env("JENSEN_HARNESS_E2E_KEY", "test-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut read_line = || {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            serde_json::from_str::<Value>(line.trim()).unwrap()
        };
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 1, "method": "session.new", "params": {"worktree": worktree_path}})
        )
        .unwrap();
        let created = read_line();
        let session_id = created["result"]["session_id"].as_str().unwrap().to_string();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc": "2.0", "id": 2, "method": "session.prompt",
                   "params": {"session_id": session_id, "message": "what is the token"}})
        )
        .unwrap();
        let prompted = read_line();
        drop(stdin);
        assert!(child.wait().unwrap().success());
        (created, prompted)
    })
    .await
    .unwrap();

    let result = &lines.1["result"];
    assert_eq!(result["stop_reason"], "end_turn", "{}", lines.1);
    assert_eq!(result["model_calls"], 2);
    assert_eq!(result["usage"]["output_tokens"], 14);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    let second: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    let tool_message = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert_eq!(tool_message["content"], "alpha\ntoken: 4417\n");
    assert_eq!(second["tools"].as_array().unwrap().len(), 6);
}

#[test]
fn the_binary_refuses_to_start_when_the_chosen_models_key_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let config = write_config(dir.path(), "http://127.0.0.1:9");

    let output = Command::new(binary_path())
        .arg("--config")
        .arg(&config)
        .arg("--state")
        .arg(dir.path().join("state.sqlite"))
        .arg("run")
        .env_remove("JENSEN_HARNESS_E2E_KEY")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("JENSEN_HARNESS_E2E_KEY"), "{stderr}");
}

#[test]
fn the_binary_reports_a_missing_config_file_by_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.toml");

    let output = Command::new(binary_path())
        .arg("--config")
        .arg(&missing)
        .arg("--state")
        .arg(dir.path().join("state.sqlite"))
        .arg("run")
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("nope.toml"));
}

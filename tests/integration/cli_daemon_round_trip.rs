//! End-to-end proof that `serve` and `run` behave identically to a client,
//! against the real binary, a real model provider, and (for `serve`) a real
//! Unix socket. Both are ignored by default and need a provider config:
//! point `JENSEN_HARNESS_LIVE_CONFIG` at a providers.toml and
//! `JENSEN_HARNESS_LIVE_MODEL` at a `provider_id/model_id` it declares, with
//! that provider's key available the way the config says. Each run asks the
//! model to read a file, so a tool call has to happen.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::common::binary_path;

struct Live {
    config: String,
    model: String,
}

fn require_live() -> Live {
    let read = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| {
            panic!("set {name} (see this file's header) and pass --ignored to run this test")
        })
    };
    Live {
        config: read("JENSEN_HARNESS_LIVE_CONFIG"),
        model: read("JENSEN_HARNESS_LIVE_MODEL"),
    }
}

fn worktree_with_a_secret_number() -> tempfile::TempDir {
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(
        worktree.path().join("notes.txt"),
        "alpha\ntoken: 4417\nomega\n",
    )
    .unwrap();
    worktree
}

fn new_session_request(worktree: &Path) -> String {
    serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "session.new",
        "params": {"worktree": worktree.to_str().unwrap()},
    })
    .to_string()
}

fn prompt_request(session_id: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session.prompt",
        "params": {
            "session_id": session_id,
            "message": "Read notes.txt with your read tool, then reply with only the number after 'token:'.",
        },
    })
    .to_string()
}

fn result_of(line: &str) -> serde_json::Value {
    let response: serde_json::Value = serde_json::from_str(line).expect("a JSON response line");
    assert!(response.get("error").is_none(), "{response}");
    response["result"].clone()
}

fn assert_a_tool_call_answered(prompt_line: &str) {
    let result = result_of(prompt_line);
    assert_eq!(result["stop_reason"], "end_turn", "{result}");
    assert!(result["model_calls"].as_u64().unwrap() >= 2, "{result}");
    assert!(
        result["usage"]["output_tokens"].as_u64().unwrap() > 0,
        "{result}"
    );
}

#[test]
#[ignore = "spawns the real binary and calls a real model; see the file header for the environment it needs"]
fn run_over_stdio_round_trips_a_real_prompt_with_a_tool_call() {
    let live = require_live();
    let worktree = worktree_with_a_secret_number();
    let mut child = Command::new(binary_path())
        .args(["--config", &live.config, "--model", &live.model, "run"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the binary starts");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));

    writeln!(stdin, "{}", new_session_request(worktree.path())).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a response line");
    let session_id = result_of(&line)["session_id"].as_str().unwrap().to_string();

    writeln!(stdin, "{}", prompt_request(&session_id)).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a response line");
    assert_a_tool_call_answered(&line);

    drop(stdin);
    child.wait().expect("the process exits once stdin closes");
}

fn wait_for_socket(path: &Path, timeout: Duration) {
    let started = Instant::now();
    while !path.exists() {
        assert!(
            started.elapsed() < timeout,
            "the daemon never created its socket at {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "spawns the real binary, binds a real socket and calls a real model; see the file header for the environment it needs"]
fn serve_over_a_unix_socket_round_trips_a_real_prompt_with_a_tool_call() {
    let live = require_live();
    let worktree = worktree_with_a_secret_number();
    let runtime_dir = tempfile::tempdir().unwrap();
    let socket_path = runtime_dir.path().join("jensen-harness.sock");
    let child = Command::new(binary_path())
        .args([
            "--config",
            &live.config,
            "--model",
            &live.model,
            "serve",
            "--socket",
        ])
        .arg(&socket_path)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("the daemon starts");
    let _guard = KillOnDrop(child);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket_path).expect("connects to the daemon");
    let mut reader = BufReader::new(stream.try_clone().expect("clones the stream"));

    writeln!(stream, "{}", new_session_request(worktree.path())).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).expect("a response line");
    let session_id = result_of(&line)["session_id"].as_str().unwrap().to_string();

    writeln!(stream, "{}", prompt_request(&session_id)).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).expect("a response line");
    assert_a_tool_call_answered(&line);
}

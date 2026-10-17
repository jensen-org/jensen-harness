//! End-to-end proof that `serve` and `run` behave identically to a client,
//! against the real binary, the real Anthropic API, and (for `serve`) a
//! real Unix socket. Both are gated behind a live `ANTHROPIC_API_KEY`,
//! since there is no fake provider at this layer: the whole point is to
//! prove a real response streams back on either transport.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_jensen-harness"))
}

fn require_live_key() {
    assert!(
        std::env::var("ANTHROPIC_API_KEY").is_ok(),
        "set ANTHROPIC_API_KEY and pass --ignored to run this test"
    );
}

fn new_session_request() -> String {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "session.new", "params": {}})
        .to_string()
}

fn prompt_request(session_id: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "session.prompt",
        "params": {"session_id": session_id, "message": "reply with exactly the word acknowledged"},
    })
    .to_string()
}

fn session_id_from(line: &str) -> String {
    let response: serde_json::Value = serde_json::from_str(line).expect("a JSON response line");
    response["result"]["session_id"]
        .as_str()
        .expect("a session_id in the result")
        .to_string()
}

fn stop_reason_from(line: &str) -> String {
    let response: serde_json::Value = serde_json::from_str(line).expect("a JSON response line");
    response["result"]["stop_reason"]
        .as_str()
        .expect("a stop_reason in the result")
        .to_string()
}

#[test]
#[ignore = "spawns the real binary and makes a real Anthropic call; run with ANTHROPIC_API_KEY set and --ignored"]
fn run_over_stdio_round_trips_a_real_prompt() {
    require_live_key();
    let mut child = Command::new(binary_path())
        .arg("run")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary starts");
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout is piped"));

    writeln!(stdin, "{}", new_session_request()).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a response line");
    let session_id = session_id_from(&line);

    writeln!(stdin, "{}", prompt_request(&session_id)).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).expect("a response line");
    let stop_reason = stop_reason_from(&line);
    assert_eq!(stop_reason, "end_turn");

    drop(stdin);
    child.wait().expect("the process exits once stdin closes");
}

fn wait_for_socket(path: &std::path::Path, timeout: Duration) {
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
#[ignore = "spawns the real binary, binds a real socket, and makes a real Anthropic call; run with ANTHROPIC_API_KEY set and --ignored"]
fn serve_over_a_unix_socket_round_trips_a_real_prompt() {
    require_live_key();
    let runtime_dir = tempfile::tempdir().unwrap();
    let socket_path = runtime_dir.path().join("jensen-harness.sock");
    let child = Command::new(binary_path())
        .arg("serve")
        .arg("--socket")
        .arg(&socket_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the daemon starts");
    let _guard = KillOnDrop(child);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let mut stream =
        std::os::unix::net::UnixStream::connect(&socket_path).expect("connects to the daemon");
    let mut reader = BufReader::new(stream.try_clone().expect("clones the stream"));

    writeln!(stream, "{}", new_session_request()).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).expect("a response line");
    let session_id = session_id_from(&line);

    writeln!(stream, "{}", prompt_request(&session_id)).unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).expect("a response line");
    let stop_reason = stop_reason_from(&line);
    assert_eq!(stop_reason, "end_turn");
}

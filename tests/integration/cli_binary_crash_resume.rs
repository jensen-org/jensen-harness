//! The design's phase 2 check against the real binary: kill -9 the daemon
//! while a model call is in flight, start it again on the same state file,
//! and resume. A completed call and a completed tool must not run twice, and
//! the ledger must not settle a call twice.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use crate::cli_binary_e2e::{completion, write_config};
use crate::common::binary_path;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

pub struct Daemon {
    pub child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Daemon {
    pub fn start(config: &Path, state: &Path) -> Self {
        Self::start_with(config, state, &[])
    }

    pub fn start_with(config: &Path, state: &Path, env: &[(&str, &str)]) -> Self {
        let mut child = Command::new(binary_path())
            .envs(env.iter().copied())
            .arg("--config")
            .arg(config)
            .arg("--state")
            .arg(state)
            .arg("run")
            .env("JENSEN_HARNESS_E2E_KEY", "test-key")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
        }
    }

    pub async fn send(&mut self, request: Value) {
        self.stdin
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    pub async fn response(&mut self, id: i64) -> Value {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the daemon closed its output");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        })
        .await
        .expect("no response in time")
    }

    pub async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        self.response(id).await
    }
}

fn tool_call_reply() -> ResponseTemplate {
    completion(
        "tool_calls",
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {
                    "name": "edit",
                    "arguments": "{\"path\":\"notes.txt\",\"find\":\"alpha\",\"replace\":\"alpha+\"}",
                },
            }],
        }),
    )
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    worktree: tempfile::TempDir,
}

fn fixture(server: &MockServer) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(worktree.path().join("notes.txt"), "alpha beta\n").unwrap();
    Fixture {
        config: write_config(dir.path(), &server.uri()),
        state: dir.path().join("state.sqlite"),
        worktree,
        _dir: dir,
    }
}

pub async fn wait_for_requests(server: &MockServer, count: usize) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if server.received_requests().await.unwrap().len() >= count {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the provider never saw the expected requests");
}

pub fn count(connection: &rusqlite::Connection, sql: &str) -> i64 {
    connection.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_mid_call_resumes_without_repeating_a_call_or_a_tool_or_a_settlement() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(tool_call_reply())
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            completion("stop", json!({"role": "assistant", "content": "never"}))
                .set_delay(Duration::from_secs(120)),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(completion(
            "stop",
            json!({"role": "assistant", "content": "all done"}),
        ))
        .mount(&server)
        .await;
    let fixture = fixture(&server);
    let worktree = fixture.worktree.path().to_str().unwrap().to_string();

    let mut first = Daemon::start(&fixture.config, &fixture.state);
    let created = first
        .call(1, "session.new", json!({"worktree": worktree}))
        .await;
    let session_id = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    first
        .send(
            json!({"jsonrpc": "2.0", "id": 2, "method": "session.prompt",
                     "params": {"session_id": session_id, "message": "mark the note"}}),
        )
        .await;
    wait_for_requests(&server, 2).await;
    first.child.kill().await.unwrap();
    drop(first);
    assert_eq!(
        std::fs::read_to_string(fixture.worktree.path().join("notes.txt")).unwrap(),
        "alpha+ beta\n",
        "the edit ran once before the kill"
    );

    let mut second = Daemon::start(&fixture.config, &fixture.state);
    let resumed = second
        .call(1, "session.resume", json!({"session_id": session_id}))
        .await;

    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_eq!(resumed["result"]["stop_reason"], "end_turn");
    assert_eq!(
        resumed["result"]["model_calls"], 2,
        "one replayed from the journal, one new"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.worktree.path().join("notes.txt")).unwrap(),
        "alpha+ beta\n",
        "the completed edit must not run a second time"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3, "the first call was not repeated");
    let body = |index: usize| -> Value { serde_json::from_slice(&requests[index].body).unwrap() };
    assert_eq!(
        body(1)["messages"],
        body(2)["messages"],
        "the resumed call must see exactly what the lost call saw"
    );
    let tool_message = body(2)["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .unwrap()
        .clone();
    assert_eq!(
        tool_message["content"],
        "edited notes.txt: 1 replacement, first at line 1"
    );

    second
        .call(
            2,
            "session.new",
            json!({"worktree": worktree, "budget": {"max_tokens": 100}}),
        )
        .await;
    drop(second);

    let db = rusqlite::Connection::open(&fixture.state).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM ledger WHERE kind = 'reserve'"),
        3
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM ledger WHERE kind = 'settle'"),
        2
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM ledger WHERE kind = 'release'"),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM (SELECT call_id FROM ledger WHERE kind IN ('settle', 'release') GROUP BY call_id HAVING COUNT(*) > 1)"
        ),
        0,
        "no call ended twice"
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM events WHERE session_id = '{session_id}' AND kind = 'model.responded'"
            )
        ),
        2
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM events WHERE session_id = '{session_id}' AND kind = 'prompt.finished'"
            )
        ),
        1
    );
    assert!(
        db.execute("UPDATE events SET payload = '{}'", []).is_err(),
        "history stays append only"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hard_cap_stops_a_run_cleanly_and_the_stop_survives_a_restart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(completion(
            "stop",
            json!({"role": "assistant", "content": "never asked"}),
        ))
        .mount(&server)
        .await;
    let fixture = fixture(&server);
    let worktree = fixture.worktree.path().to_str().unwrap().to_string();

    let mut daemon = Daemon::start(&fixture.config, &fixture.state);
    let created = daemon
        .call(
            1,
            "session.new",
            json!({"worktree": worktree, "budget": {"max_tokens": 1000}}),
        )
        .await;
    let session_id = created["result"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    let capped = daemon
        .call(
            2,
            "session.prompt",
            json!({"session_id": session_id, "message": "go"}),
        )
        .await;

    assert_eq!(
        capped["result"]["stop_reason"], "budget_exhausted",
        "{capped}"
    );
    assert_eq!(capped["result"]["model_calls"], 0);
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "no money was spent"
    );
    daemon.child.kill().await.unwrap();
    drop(daemon);

    let mut restarted = Daemon::start(&fixture.config, &fixture.state);
    let nothing = restarted
        .call(1, "session.resume", json!({"session_id": session_id}))
        .await;
    assert_eq!(
        nothing["error"]["code"], -32602,
        "a stopped prompt is finished, not resumable"
    );
}

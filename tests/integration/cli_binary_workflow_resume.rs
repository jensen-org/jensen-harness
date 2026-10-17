//! The design's phase 3 check against the real binary: kill -9 the daemon
//! while a workflow's agent node is waiting on the model, start it again on
//! the same state file and resume the run. Finished nodes must not run
//! again, the agent must pick its own session up, the provider must see each
//! completed call once, and the final channels must equal those of a run
//! nothing interrupted.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli_binary_crash_resume::{Daemon, count, wait_for_requests};
use crate::cli_binary_e2e::completion;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FLOW: &str = r#"
version: 1
name: resume
channels:
  prepared: { schema: { type: object } }
  findings: { schema: { type: array, items: { type: string } }, reducer: append }
nodes:
  prep: { kind: command, run: 'echo prepared >> prep.log', output: prepared }
  scout:
    kind: agent
    role: explore
    prompt: Read notes.txt and report the token.
    reads: [prepared]
    writes: [findings]
    max_turns: 5
edges:
  - { from: prep, to: scout }
"#;

fn read_call() -> ResponseTemplate {
    completion(
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
    )
}

fn answer() -> ResponseTemplate {
    completion(
        "stop",
        json!({"role": "assistant", "content": "{\"findings\": [\"4417\"]}"}),
    )
}

async fn mount(server: &MockServer, replies: Vec<ResponseTemplate>) {
    for reply in replies {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(reply)
            .up_to_n_times(1)
            .mount(server)
            .await;
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    worktree: tempfile::TempDir,
}

fn setup(server: &MockServer) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(worktree.path().join("notes.txt"), "alpha\ntoken: 4417\n").unwrap();
    std::fs::write(worktree.path().join("flow.yaml"), FLOW).unwrap();
    let config = dir.path().join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "default_model = \"fake/some-model\"\n\n[roles]\nexplore = \"fake/some-model\"\n\n[providers.fake]\ntransport = \"openai_compatible\"\nbase_url = \"{}/v1\"\napi_key_env = \"JENSEN_HARNESS_E2E_KEY\"\n",
            server.uri()
        ),
    )
    .unwrap();
    Setup {
        config,
        state: dir.path().join("state.sqlite"),
        worktree,
        _dir: dir,
    }
}

fn point_at(setup: &Setup, server: &MockServer) {
    let text = std::fs::read_to_string(&setup.config).unwrap();
    let rewritten: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with("base_url") {
                format!("base_url = \"{}/v1\"", server.uri())
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(&setup.config, rewritten.join("\n")).unwrap();
}

fn start_params(setup: &Setup) -> Value {
    json!({"worktree": setup.worktree.path().to_str().unwrap(), "workflow": "flow.yaml"})
}

fn prep_log(path: &Path) -> String {
    std::fs::read_to_string(path.join("prep.log")).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workflow_killed_mid_agent_call_resumes_to_the_same_channels_as_an_uninterrupted_run() {
    let reference_server = MockServer::start().await;
    mount(&reference_server, vec![read_call(), answer()]).await;
    let reference = setup(&reference_server);
    let mut daemon = Daemon::start(&reference.config, &reference.state);
    let uninterrupted = daemon
        .call(1, "workflow.start", start_params(&reference))
        .await;
    assert!(uninterrupted.get("error").is_none(), "{uninterrupted}");
    assert_eq!(uninterrupted["result"]["status"], "succeeded");
    drop(daemon);
    {
        use std::os::unix::fs::PermissionsExt;
        let checkpoints = reference.state.parent().unwrap().join("workflow-runs");
        let mode = std::fs::metadata(checkpoints).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "checkpoints hold channel state");
    }

    let first_server = MockServer::start().await;
    mount(
        &first_server,
        vec![read_call(), answer().set_delay(Duration::from_secs(120))],
    )
    .await;
    let run = setup(&first_server);
    let mut first = Daemon::start(&run.config, &run.state);
    first
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "workflow.start", "params": start_params(&run)}))
        .await;
    wait_for_requests(&first_server, 2).await;
    first.child.kill().await.unwrap();
    drop(first);
    assert_eq!(prep_log(run.worktree.path()), "prepared\n");

    let db = rusqlite::Connection::open(&run.state).unwrap();
    let run_id: String = db
        .query_row("SELECT run_id FROM workflow_runs", [], |row| row.get(0))
        .unwrap();
    drop(db);

    let second_server = MockServer::start().await;
    mount(&second_server, vec![answer()]).await;
    point_at(&run, &second_server);
    let mut second = Daemon::start(&run.config, &run.state);
    let status = second
        .call(1, "workflow.status", json!({"run_id": run_id}))
        .await;
    assert_eq!(status["result"]["status"], "running", "{status}");
    let resumed = second
        .call(2, "workflow.resume", json!({"run_id": run_id}))
        .await;
    drop(second);

    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_eq!(resumed["result"]["status"], "succeeded", "{resumed}");
    assert_eq!(
        resumed["result"]["channels"]["findings"], uninterrupted["result"]["channels"]["findings"],
        "the resumed run ends where an uninterrupted one does"
    );
    assert_eq!(
        resumed["result"]["channels"]["prepared"]["stdout"],
        uninterrupted["result"]["channels"]["prepared"]["stdout"]
    );
    assert_eq!(
        prep_log(run.worktree.path()),
        "prepared\n",
        "the finished node was not run again"
    );
    assert_eq!(first_server.received_requests().await.unwrap().len(), 2);
    let resumed_requests = second_server.received_requests().await.unwrap();
    assert_eq!(resumed_requests.len(), 1, "only the lost call was repeated");
    let body: Value = serde_json::from_slice(&resumed_requests[0].body).unwrap();
    let tool_result = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert!(
        tool_result["content"].as_str().unwrap().contains("4417"),
        "the finished tool result was replayed from the journal"
    );

    let db = rusqlite::Connection::open(&run.state).unwrap();
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
                "SELECT COUNT(*) FROM events WHERE session_id = 'wf:{run_id}' AND kind = 'node.finished'"
            )
        ),
        2
    );
}

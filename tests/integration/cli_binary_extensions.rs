use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli_binary_crash_resume::{Daemon, count};
use crate::cli_binary_e2e::completion;
use crate::common::testkit_mcp_path;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct World {
    _dir: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    worktree: PathBuf,
    log: PathBuf,
}

fn tool_call(name: &str, arguments: &str) -> ResponseTemplate {
    completion(
        "tool_calls",
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": name, "arguments": arguments},
            }],
        }),
    )
}

fn answer(text: &str) -> ResponseTemplate {
    completion("stop", json!({"role": "assistant", "content": text}))
}

async fn once(server: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

fn world(provider: &MockServer) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let worktree = root.join("project");
    std::fs::create_dir_all(worktree.join("src")).unwrap();
    std::fs::write(
        worktree.join("src/lib.rs"),
        "pub fn alpha() -> u32 { 1 }\npub fn beta() -> u32 { 2 }\n",
    )
    .unwrap();
    std::fs::copy(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/workflows/examples/design.yaml"
        ),
        worktree.join("design.yaml"),
    )
    .unwrap();
    let log = worktree.join("mcp.log");
    let server = testkit_mcp_path();
    let library = concat!(env!("CARGO_MANIFEST_DIR"), "/agents");
    let config = root.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            r#"default_model = "fake/some-model"

[roles]
architect = "fake/some-model"
design = "fake/some-model"

[library]
paths = ["{library}"]

[providers.fake]
transport = "openai_compatible"
base_url = "{}/v1"
api_key_env = "JENSEN_HARNESS_E2E_KEY"

[mcp.servers.excalidraw]
command = "{server}"
env_allow = ["TESTKIT_LOG", "TESTKIT_DRAW_DELAY", "TESTKIT_ECHO_DELAY"]
access = "read_write"

[mcp.servers.figma]
command = "{server}"
env_allow = ["TESTKIT_LOG"]
access = "read_only"
"#,
            provider.uri(),
            server = server.display(),
        ),
    )
    .unwrap();
    World {
        config,
        state: root.join("state.sqlite"),
        worktree,
        log,
        _dir: dir,
    }
}

async fn wait_for_log(log: &Path, wanted: &str) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if std::fs::read_to_string(log).is_ok_and(|text| text.contains(wanted)) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the tool never ran");
}

fn bodies(requests: &[wiremock::Request]) -> Vec<Value> {
    requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect()
}

fn lines(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

async fn killed_during(
    tool: &str,
    arguments: &str,
    delay_variable: &str,
    wait_for: &str,
) -> (World, MockServer, String) {
    let first = MockServer::start().await;
    once(&first, tool_call(tool, arguments)).await;
    once(
        &first,
        answer("never delivered").set_delay(Duration::from_secs(120)),
    )
    .await;
    let world = world(&first);
    let log = world.log.display().to_string();
    let mut daemon = Daemon::start_with(
        &world.config,
        &world.state,
        &[("TESTKIT_LOG", log.as_str()), (delay_variable, "120")],
    );
    daemon
        .send(
            json!({"jsonrpc": "2.0", "id": 1, "method": "workflow.start",
            "params": {"worktree": world.worktree.to_str().unwrap(), "workflow": "design.yaml",
                       "inputs": {"task": "design a login screen"}}}),
        )
        .await;
    wait_for_log(&world.log, wait_for).await;
    daemon.child.kill().await.unwrap();
    drop(daemon);
    let db = rusqlite::Connection::open(&world.state).unwrap();
    let run: String = db
        .query_row("SELECT run_id FROM workflow_runs", [], |row| row.get(0))
        .unwrap();
    (world, first, run)
}

async fn resumed(world: &World, run: &str) -> (Value, MockServer) {
    let second = MockServer::start().await;
    once(
        &second,
        answer(r#"{"plan": {"steps": ["a"]}, "diagram": {"nodes": 1}}"#),
    )
    .await;
    once(&second, answer(r#"{"design": {"screens": 2}}"#)).await;
    let text = std::fs::read_to_string(&world.config).unwrap();
    let rewritten: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with("base_url") {
                format!("base_url = \"{}/v1\"", second.uri())
            } else {
                line.to_owned()
            }
        })
        .collect();
    std::fs::write(&world.config, rewritten.join("\n")).unwrap();
    let log = world.log.display().to_string();
    let mut daemon = Daemon::start_with(
        &world.config,
        &world.state,
        &[("TESTKIT_LOG", log.as_str())],
    );
    let resumed = daemon
        .call(1, "workflow.resume", json!({"run_id": run}))
        .await;
    drop(daemon);
    (resumed, second)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_mutating_mcp_call_cut_by_a_kill_is_reported_to_the_model_and_never_repeated() {
    let (world, first, run) = killed_during(
        "mcp__excalidraw__draw",
        r#"{"text":"box"}"#,
        "TESTKIT_DRAW_DELAY",
        "draw:box",
    )
    .await;
    assert_eq!(lines(&world.log), ["draw:box"]);
    let before = bodies(&first.received_requests().await.unwrap());
    let architect_tools: Vec<&str> = before[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(
        architect_tools.contains(&"mcp__excalidraw__draw"),
        "{architect_tools:?}"
    );
    assert!(
        !architect_tools
            .iter()
            .any(|name| name.starts_with("mcp__figma"))
    );
    assert!(
        before[0]["messages"].to_string().contains("fn alpha"),
        "the eager tree sitter context"
    );

    let (outcome, second) = resumed(&world, &run).await;

    assert!(outcome.get("error").is_none(), "{outcome}");
    assert_eq!(outcome["result"]["status"], "succeeded", "{outcome}");
    assert_eq!(
        outcome["result"]["channels"]["design"],
        json!({"screens": 2})
    );
    assert_eq!(
        outcome["result"]["channels"]["plan"],
        json!({"steps": ["a"]})
    );
    assert_eq!(
        lines(&world.log),
        ["draw:box"],
        "the drawing must not be made twice"
    );
    let after = bodies(&second.received_requests().await.unwrap());
    assert!(
        after[0]["messages"]
            .to_string()
            .contains("interrupted by a restart"),
        "the model must be told the call was cut"
    );
    let designer_tools: Vec<&str> = after[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert!(
        designer_tools.contains(&"skill") && designer_tools.contains(&"mcp__figma__echo"),
        "{designer_tools:?}"
    );
    assert!(
        !designer_tools.contains(&"mcp__figma__draw"),
        "a read only server must hide mutating tools"
    );
    let designer_system = after[1]["messages"][0]["content"].to_string();
    assert!(
        designer_system.contains("design-review: Check a design"),
        "{designer_system}"
    );
    assert!(
        after[1]["messages"]
            .to_string()
            .contains("answer of node architect")
    );

    let db = rusqlite::Connection::open(&world.state).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM ledger WHERE kind = 'settle'"),
        3
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM (SELECT call_id FROM ledger WHERE kind IN ('settle', 'release') GROUP BY call_id HAVING COUNT(*) > 1)"
        ),
        0
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM events WHERE session_id = '{run}.architect.0' AND kind = 'prompt.started'"
            )
        ),
        1,
        "the gathered context was not gathered and prompted twice"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_only_mcp_call_cut_by_a_kill_is_run_again() {
    let (world, _first, run) = killed_during(
        "mcp__excalidraw__echo",
        r#"{"text":"ping"}"#,
        "TESTKIT_ECHO_DELAY",
        "echo:ping",
    )
    .await;
    assert_eq!(lines(&world.log), ["echo:ping"]);

    let (outcome, second) = resumed(&world, &run).await;

    assert_eq!(outcome["result"]["status"], "succeeded", "{outcome}");
    assert_eq!(
        lines(&world.log),
        ["echo:ping", "echo:ping"],
        "a read only call is repeated"
    );
    let after = bodies(&second.received_requests().await.unwrap());
    let seen = after[0]["messages"].to_string();
    assert!(
        seen.contains("ping") && !seen.contains("interrupted by a restart"),
        "{seen}"
    );
}

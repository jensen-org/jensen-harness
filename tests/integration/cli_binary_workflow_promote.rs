use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::cli_binary_crash_resume::{Daemon, count, wait_for_requests};
use crate::cli_binary_e2e::completion;
use crate::promote_worktree::git;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FLOW: &str = r#"
version: 1
name: implement
channels:
  task: { schema: { type: string }, init: "" }
  checked: { schema: { type: boolean }, verified: true }
  promoted: { schema: { type: object } }
  published: { schema: { type: object } }
nodes:
  write: { kind: agent, role: implement, prompt: Mark the note, reads: [task], tools: writer, max_turns: 5 }
  check: { kind: verifier, run: "grep -q 'alpha+' a.txt", pass_when: exit_code == 0, passes: checked }
  promote: { kind: promote, from: write, requires: [checked], output: promoted }
  publish: { kind: publish, from: promote, title: Mark the note, body: Automated, output: published }
edges:
  - { from: write, to: check }
  - { from: check, to: promote }
  - { from: promote, to: publish }
"#;

struct World {
    _dir: tempfile::TempDir,
    config: PathBuf,
    state: PathBuf,
    repo: PathBuf,
    origin: PathBuf,
}

fn edit_call() -> ResponseTemplate {
    completion(
        "tool_calls",
        json!({
            "role": "assistant",
            "content": null,
            "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "edit", "arguments": "{\"path\":\"a.txt\",\"find\":\"alpha\",\"replace\":\"alpha+\"}"},
            }],
        }),
    )
}

fn done() -> ResponseTemplate {
    completion("stop", json!({"role": "assistant", "content": "done"}))
}

async fn once(server: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(template)
        .up_to_n_times(1)
        .mount(server)
        .await;
}

fn pull(number: u64) -> Value {
    json!({
        "url": format!("https://api.example.invalid/repos/acme/app/pulls/{number}"),
        "id": number,
        "number": number,
        "state": "open",
        "title": "Mark the note",
        "html_url": format!("https://github.example.invalid/acme/app/pull/{number}"),
        "head": {"label": "acme:x", "ref": "x", "sha": "0000000000000000000000000000000000000000"},
        "base": {"label": "acme:main", "ref": "main", "sha": "0000000000000000000000000000000000000000"},
    })
}

fn world(provider: &MockServer, forge: &MockServer) -> World {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("a.txt"), "alpha\n").unwrap();
    std::fs::write(repo.join("flow.yaml"), FLOW).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let origin = root.join("origin.git");
    git(&root, &["init", "-q", "--bare", "origin.git"]);
    git(
        &repo,
        &["remote", "add", "origin", "http://127.0.0.1/acme/app.git"],
    );
    git(
        &repo,
        &["config", "remote.origin.pushurl", origin.to_str().unwrap()],
    );
    let config = root.join("providers.toml");
    std::fs::write(
        &config,
        format!(
            "default_model = \"fake/some-model\"\n\n[roles]\nimplement = \"fake/some-model\"\n\n[providers.fake]\ntransport = \"openai_compatible\"\nbase_url = \"{}/v1\"\napi_key_env = \"JENSEN_HARNESS_E2E_KEY\"\n\n[forges.github]\nbase_url = \"{}\"\napi_key_env = \"JENSEN_HARNESS_E2E_KEY\"\n",
            provider.uri(),
            forge.uri()
        ),
    )
    .unwrap();
    World {
        config,
        state: root.join("state.sqlite"),
        repo,
        origin,
        _dir: dir,
    }
}

fn start_params(world: &World) -> Value {
    json!({"worktree": world.repo.to_str().unwrap(), "workflow": "flow.yaml"})
}

fn run_id(world: &World) -> String {
    let db = rusqlite::Connection::open(&world.state).unwrap();
    db.query_row("SELECT run_id FROM workflow_runs", [], |row| row.get(0))
        .unwrap()
}

async fn posts(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|request| request.method.as_str() == "POST")
        .count()
}

fn commits(repo: &Path, branch: &str) -> i64 {
    git(repo, &["rev-list", "--count", &format!("main..{branch}")])
        .parse()
        .unwrap()
}

async fn wait_for_post(server: &MockServer) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while posts(server).await == 0 {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the forge never saw a POST");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_during_the_writer_promotes_and_publishes_once_after_resume() {
    let provider = MockServer::start().await;
    once(&provider, edit_call()).await;
    once(&provider, done().set_delay(Duration::from_secs(120))).await;
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(ResponseTemplate::new(201).set_body_json(pull(7)))
        .mount(&forge)
        .await;
    let world = world(&provider, &forge);

    let mut first = Daemon::start(&world.config, &world.state);
    first
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "workflow.start", "params": start_params(&world)}))
        .await;
    wait_for_requests(&provider, 2).await;
    first.child.kill().await.unwrap();
    drop(first);
    let run = run_id(&world);
    let branch = format!("jensen/{run}/integration");
    assert_eq!(
        commits(&world.repo, &branch),
        0,
        "nothing was promoted before the kill"
    );

    once(&provider, done()).await;
    let mut second = Daemon::start(&world.config, &world.state);
    let resumed = second
        .call(1, "workflow.resume", json!({"run_id": run}))
        .await;
    drop(second);

    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_eq!(resumed["result"]["status"], "succeeded", "{resumed}");
    assert_eq!(
        resumed["result"]["integration_branch"],
        format!("refs/heads/{branch}")
    );
    assert_eq!(commits(&world.repo, &branch), 1);
    assert_eq!(
        resumed["result"]["head"].as_str().unwrap(),
        git(&world.repo, &["rev-parse", &branch])
    );
    let message = git(&world.repo, &["log", "-1", "--format=%B", &branch]);
    assert!(message.contains("Jensen-Node: write"), "{message}");
    assert_eq!(
        git(&world.repo, &["show", &format!("{branch}:a.txt")]),
        "alpha+"
    );
    assert_eq!(
        std::fs::read_to_string(world.repo.join("a.txt")).unwrap(),
        "alpha\n"
    );
    assert_eq!(
        git(&world.repo, &["symbolic-ref", "--short", "HEAD"]),
        "main"
    );
    assert_eq!(
        git(
            &world.origin,
            &["rev-parse", &format!("refs/heads/jensen/{run}")]
        ),
        git(&world.repo, &["rev-parse", &branch])
    );
    assert_eq!(resumed["result"]["channels"]["published"]["number"], 7);
    assert_eq!(posts(&forge).await, 1);

    let db = rusqlite::Connection::open(&world.state).unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM ledger WHERE kind = 'settle'"),
        2
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM (SELECT call_id FROM ledger WHERE kind IN ('settle', 'release') GROUP BY call_id HAVING COUNT(*) > 1)"
        ),
        0
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_killed_between_the_push_and_the_forge_call_opens_exactly_one_pull_request() {
    let provider = MockServer::start().await;
    once(&provider, edit_call()).await;
    once(&provider, done()).await;
    let forge = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .up_to_n_times(1)
        .mount(&forge)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([pull(7)])))
        .mount(&forge)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/app/pulls"))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(pull(7))
                .set_delay(Duration::from_secs(120)),
        )
        .mount(&forge)
        .await;
    let world = world(&provider, &forge);

    let mut first = Daemon::start(&world.config, &world.state);
    first
        .send(json!({"jsonrpc": "2.0", "id": 1, "method": "workflow.start", "params": start_params(&world)}))
        .await;
    wait_for_post(&forge).await;
    first.child.kill().await.unwrap();
    drop(first);
    let run = run_id(&world);
    let branch = format!("jensen/{run}/integration");
    assert_eq!(
        commits(&world.repo, &branch),
        1,
        "promotion finished before the kill"
    );
    assert_eq!(
        git(
            &world.origin,
            &["rev-parse", &format!("refs/heads/jensen/{run}")]
        ),
        git(&world.repo, &["rev-parse", &branch]),
        "the branch was pushed before the kill"
    );

    let mut second = Daemon::start(&world.config, &world.state);
    let resumed = second
        .call(1, "workflow.resume", json!({"run_id": run}))
        .await;
    drop(second);

    assert!(resumed.get("error").is_none(), "{resumed}");
    assert_eq!(resumed["result"]["status"], "succeeded", "{resumed}");
    assert_eq!(resumed["result"]["channels"]["published"]["existing"], true);
    assert_eq!(posts(&forge).await, 1, "no second pull request was opened");
    assert_eq!(
        commits(&world.repo, &branch),
        1,
        "promotion was not repeated"
    );

    let db = rusqlite::Connection::open(&world.state).unwrap();
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM events WHERE session_id = 'wf:{run}' AND kind = 'publish.started'"
            )
        ),
        2
    );
    assert_eq!(
        count(
            &db,
            &format!(
                "SELECT COUNT(*) FROM events WHERE session_id = 'wf:{run}' AND kind = 'publish.finished'"
            )
        ),
        1
    );
}

use std::sync::Arc;
use std::time::Duration;

use crate::common::{Scripted, says};
use jensen_harness::engine::Engine;
use jensen_harness::rpc::serve_connection;
use jensen_harness_protocol::{
    Response, WorkflowCancelResult, WorkflowRunResult, WorkflowStatus, WorkflowStatusResult,
    error_code,
};
use serde_json::{Value, json};
use tokio::io::BufReader;

const FLOW: &str = r#"
version: 1
name: rpc
channels:
  findings: { schema: { type: array, items: { type: string } }, reducer: append }
  built: { schema: { type: object } }
nodes:
  scout: { kind: agent, role: explore, prompt: look, writes: [findings], max_turns: 3 }
  build: { kind: command, run: echo built, reads: [findings], output: built }
edges:
  - { from: scout, to: build }
"#;

async fn call(engine: &Arc<Engine>, request: Value) -> Response {
    let input = format!("{request}\n");
    let reader = BufReader::new(input.as_bytes());
    let mut output = Vec::new();
    serve_connection(reader, &mut output, Arc::clone(engine))
        .await
        .unwrap();
    serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap()
}

fn request(id: i64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

fn engine_with(replies: Vec<jensen_harness_core::providers::CompletionResponse>) -> Arc<Engine> {
    Arc::new(Engine::with_fixed_model(Arc::new(Scripted::of(replies)), "m").unwrap())
}

#[tokio::test]
async fn start_runs_a_workflow_file_and_status_reports_it_finished() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("flow.yaml"), FLOW).unwrap();
    let engine = engine_with(vec![says(r#"{"findings": ["a"]}"#)]);

    let response = call(
        &engine,
        request(
            1,
            "workflow.start",
            json!({"worktree": dir.path().to_str().unwrap(), "workflow": "flow.yaml"}),
        ),
    )
    .await;

    let result: WorkflowRunResult = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(
        result.status,
        WorkflowStatus::Succeeded,
        "{:?}",
        result.failure
    );
    assert_eq!(result.channels.get("findings"), Some(&json!(["a"])));
    assert_eq!(result.outcomes.get("build").map(String::as_str), Some("ok"));

    let status = call(
        &engine,
        request(2, "workflow.status", json!({"run_id": result.run_id})),
    )
    .await;
    let status: WorkflowStatusResult = serde_json::from_value(status.result.unwrap()).unwrap();
    assert_eq!(status.status, WorkflowStatus::Succeeded);
    assert_eq!(status.spec_hash.len(), 64);

    let again = call(
        &engine,
        request(3, "workflow.resume", json!({"run_id": status.run_id})),
    )
    .await;
    let again: WorkflowRunResult = serde_json::from_value(again.result.unwrap()).unwrap();
    assert_eq!(again.status, WorkflowStatus::Succeeded);
    assert_eq!(again.channels, result.channels);
}

#[tokio::test]
async fn bad_workflow_requests_are_invalid_params_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("loop.yaml"),
        "version: 1\nname: x\nnodes:\n  c: {kind: command, run: x}\n  l: {kind: loop, body: [c], until: 'true'}\n",
    )
    .unwrap();
    let engine = engine_with(vec![]);
    let worktree = dir.path().to_str().unwrap();
    let cases = [
        request(
            1,
            "workflow.start",
            json!({"worktree": worktree, "workflow": "missing.yaml"}),
        ),
        request(
            2,
            "workflow.start",
            json!({"worktree": worktree, "workflow": "loop.yaml"}),
        ),
        request(
            3,
            "workflow.start",
            json!({"worktree": worktree, "workflow": "loop.yaml", "typo": 1}),
        ),
        request(4, "workflow.status", json!({"run_id": "nope"})),
        request(5, "workflow.resume", json!({"run_id": "nope"})),
    ];
    for case in cases {
        let response = call(&engine, case.clone()).await;
        let error = response
            .error
            .unwrap_or_else(|| panic!("{case} was accepted"));
        assert_eq!(
            error.code,
            error_code::INVALID_PARAMS,
            "{case}: {}",
            error.message
        );
    }
    let unbounded = call(
        &engine,
        request(
            2,
            "workflow.start",
            json!({"worktree": worktree, "workflow": "loop.yaml"}),
        ),
    )
    .await;
    assert!(unbounded.error.unwrap().message.contains("unbounded"));
}

#[tokio::test]
async fn cancel_stops_a_run_in_flight_and_an_unknown_run_is_not_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("slow.yaml"),
        "version: 1\nname: slow\nchannels:\n  out: {schema: {type: object}}\nnodes:\n  wait: {kind: command, run: 'sleep 30', output: out, timeout_secs: 60}\n",
    )
    .unwrap();
    let engine = engine_with(vec![]);
    let started = {
        let engine = Arc::clone(&engine);
        let worktree = dir.path().to_str().unwrap().to_owned();
        tokio::spawn(async move {
            call(
                &engine,
                request(
                    1,
                    "workflow.start",
                    json!({"worktree": worktree, "workflow": "slow.yaml"}),
                ),
            )
            .await
        })
    };
    let run_id = loop {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let active = engine.active_workflows.lock().await;
        if let Some(run_id) = active.keys().next() {
            break run_id.clone();
        }
    };
    tokio::time::sleep(Duration::from_millis(200)).await;

    let cancelled = call(
        &engine,
        request(2, "workflow.cancel", json!({"run_id": run_id})),
    )
    .await;
    let cancelled: WorkflowCancelResult =
        serde_json::from_value(cancelled.result.unwrap()).unwrap();
    assert!(cancelled.cancelled);

    let ended = tokio::time::timeout(Duration::from_secs(10), started)
        .await
        .expect("a cancelled run ends promptly")
        .unwrap();
    let ended: WorkflowRunResult = serde_json::from_value(ended.result.unwrap()).unwrap();
    assert_eq!(ended.status, WorkflowStatus::Cancelled);

    let unknown = call(
        &engine,
        request(3, "workflow.cancel", json!({"run_id": "nope"})),
    )
    .await;
    let unknown: WorkflowCancelResult = serde_json::from_value(unknown.result.unwrap()).unwrap();
    assert!(!unknown.cancelled);
}

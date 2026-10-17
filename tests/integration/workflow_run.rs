use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use dagrs::{MemoryRunStore, RunStore};
use jensen_harness_core::budget::{BudgetSettings, Caps};
use jensen_harness_core::providers::{
    CompletionRequest, CompletionResponse, Provider, ProviderError,
};
use jensen_harness_core::store::{EventKind, NewEvent, Store};
use jensen_harness_core::toolset::ToolSet;
use jensen_harness_workflow::nodes::{self, Values};
use jensen_harness_workflow::runtime::Shared;
use jensen_harness_workflow::{
    Host, RoleBinding, RunError, RunReport, RunStatus, Runner, StartRequest, load,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::common::{Scripted, says, tool_set};

struct TestHost {
    provider: Arc<dyn Provider>,
    dir: PathBuf,
    budget: BudgetSettings,
}

impl Host for TestHost {
    fn bind(&self, _role: &str) -> Result<RoleBinding, RunError> {
        Ok(RoleBinding {
            provider: Arc::clone(&self.provider),
            model_address: "scripted/m".to_owned(),
        })
    }

    fn tools(&self) -> Result<ToolSet, RunError> {
        Ok(tool_set(&self.dir))
    }

    fn budget(&self) -> &BudgetSettings {
        &self.budget
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    store: Store,
    host: Arc<TestHost>,
}

fn fixture(provider: Arc<dyn Provider>) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let store = Store::in_memory().unwrap();
    let host = Arc::new(TestHost {
        provider,
        dir: root.clone(),
        budget: BudgetSettings::default(),
    });
    Fixture {
        _dir: dir,
        root,
        store,
        host,
    }
}

fn runner(fixture: &Fixture) -> Runner {
    Runner {
        store: fixture.store.clone(),
        host: fixture.host.clone(),
        checkpoints: Arc::new(MemoryRunStore::new()) as Arc<dyn RunStore>,
    }
}

async fn run(fixture: &Fixture, yaml: &str, run_id: &str, caps: Caps) -> RunReport {
    runner(fixture)
        .start(
            load(yaml).unwrap(),
            StartRequest {
                run_id: run_id.to_owned(),
                inputs: Values::new(),
                caps,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap()
}

fn scripted(replies: Vec<CompletionResponse>) -> Arc<dyn Provider> {
    Arc::new(Scripted::of(replies))
}

const ALL_KINDS: &str = r#"
version: 1
name: all-kinds
channels:
  findings: { schema: { type: array, items: { type: string } }, reducer: append }
  built: { schema: { type: object } }
  ok: { schema: { type: boolean }, verified: true }
  shipped: { schema: { type: object } }
  fixed: { schema: { type: object } }
nodes:
  scout: { kind: agent, role: explore, prompt: look around, writes: [findings], max_turns: 3 }
  build: { kind: command, run: echo built, reads: [findings], output: built }
  check: { kind: verifier, run: "true", reads: [built], pass_when: exit_code == 0, passes: ok }
  route: { kind: router, reads: [ok], expr: 'channels.ok ? "ship" : "fix"' }
  ship: { kind: command, run: echo shipped, output: shipped }
  fix: { kind: command, run: echo fixed, output: fixed }
edges:
  - { from: scout, to: build }
  - { from: build, to: check }
  - { from: check, to: route }
  - { from: route, to: ship, when: 'label == "ship"' }
  - { from: route, to: fix, when: 'label == "fix"' }
"#;

#[tokio::test]
async fn every_node_kind_runs_and_a_router_prunes_the_other_branch() {
    let fixture = fixture(scripted(vec![says(r#"{"findings": ["a"]}"#)]));
    let report = run(&fixture, ALL_KINDS, "r1", Caps::default()).await;
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(report.channels.get("findings"), Some(&json!(["a"])));
    assert_eq!(report.channels.get("ok"), Some(&json!(true)));
    let shipped = report.channels.get("shipped").unwrap();
    assert_eq!(shipped.get("stdout"), Some(&json!("shipped\n")));
    assert!(!report.channels.contains_key("fixed"));
    assert_eq!(report.outcomes.get("ship").map(String::as_str), Some("ok"));
    assert!(!report.outcomes.contains_key("fix"));
    let events = fixture.store.events("wf:r1").await.unwrap();
    assert!(events.iter().any(|e| e.kind == EventKind::NodeSkipped));
    assert_eq!(events.first().unwrap().kind, EventKind::WorkflowStarted);
    assert_eq!(events.last().unwrap().kind, EventKind::WorkflowFinished);
}

#[tokio::test]
async fn an_agent_node_spends_from_its_node_scope_into_the_run_scope() {
    let fixture = fixture(scripted(vec![says(r#"{"findings": ["a"]}"#)]));
    run(&fixture, ALL_KINDS, "r2", Caps::default()).await;
    let chain = fixture.store.usage("r2:scout:0").await.unwrap();
    let node = chain.iter().find(|u| u.scope_id == "r2:scout:0").unwrap();
    let run_scope = chain.iter().find(|u| u.scope_id == "r2").unwrap();
    assert!(node.spent_tokens > 0);
    assert_eq!(node.spent_tokens, run_scope.spent_tokens);
}

#[tokio::test]
async fn a_hard_cap_on_the_run_ends_it_as_budget_exceeded() {
    let fixture = fixture(scripted(vec![says(r#"{"findings": ["a"]}"#)]));
    let caps = Caps {
        max_tokens: Some(50),
        max_cost_micro_usd: None,
    };
    let report = run(&fixture, ALL_KINDS, "r3", caps).await;
    assert_eq!(report.status, RunStatus::BudgetExceeded);
    assert!(matches!(
        report.failure,
        Some(RunError::BudgetExhausted { .. })
    ));
    assert!(
        fixture
            .store
            .open_reservations("r3")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_agent_reply_that_breaks_the_channel_schema_fails_the_run_loudly() {
    let fixture = fixture(scripted(vec![says(r#"{"findings": [1, 2]}"#)]));
    let report = run(&fixture, ALL_KINDS, "r4", Caps::default()).await;
    assert_eq!(report.status, RunStatus::Failed);
    assert!(report.failure.is_some());
    assert!(!report.channels.contains_key("findings"));
}

#[tokio::test]
async fn parallel_read_only_agents_merge_through_the_reducer() {
    let provider: Arc<dyn Provider> = Arc::new(Scripted::repeating(says(r#"{"findings": ["x"]}"#)));
    let fixture = fixture(provider);
    let yaml = r#"
version: 1
name: fan
channels:
  findings: { schema: { type: array }, reducer: append }
  count: { schema: { type: number } }
nodes:
  left: { kind: agent, role: r, prompt: p, writes: [findings], max_turns: 3 }
  right: { kind: agent, role: r, prompt: p, writes: [findings], max_turns: 3 }
  tally: { kind: command, run: 'echo {\"count\":1}', reads: [findings], writes: [count] }
edges:
  - { from: left, to: tally }
  - { from: right, to: tally }
"#;
    let report = run(&fixture, yaml, "r5", Caps::default()).await;
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.channels.get("findings"), Some(&json!(["x", "x"])));
    assert_eq!(report.channels.get("count"), Some(&json!(1)));
}

const COUNTING_LOOP: &str = r#"
version: 1
name: looped
channels:
  count: { schema: { type: integer } }
nodes:
  step:
    kind: command
    run: 'n=$(cat c 2>/dev/null || echo 0); echo $((n+1)) > c; echo "{\"count\": $((n+1))}"'
    writes: [count]
    idempotent: true
  again: { kind: loop, body: [step], until: UNTIL, max_iterations: MAX }
"#;

fn counting(until: &str, max: u32) -> String {
    COUNTING_LOOP
        .replace("UNTIL", &format!("'{until}'"))
        .replace("MAX", &max.to_string())
}

#[tokio::test]
async fn a_loop_ends_on_its_condition() {
    let fixture = fixture(scripted(vec![]));
    let report = run(
        &fixture,
        &counting("channels.count >= 3", 10),
        "l1",
        Caps::default(),
    )
    .await;
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.channels.get("count"), Some(&json!(3)));
    assert_eq!(report.outcomes.get("again").map(String::as_str), Some("ok"));
}

#[tokio::test]
async fn a_loop_that_hits_its_bound_is_exhausted_not_stuck() {
    let fixture = fixture(scripted(vec![]));
    let report = run(
        &fixture,
        &counting("channels.count >= 99", 2),
        "l2",
        Caps::default(),
    )
    .await;
    assert_eq!(report.channels.get("count"), Some(&json!(2)));
    assert_eq!(
        report.outcomes.get("again").map(String::as_str),
        Some("exhausted")
    );
}

#[tokio::test]
async fn a_loop_whose_iterations_repeat_themselves_is_stalled() {
    let fixture = fixture(scripted(vec![]));
    let yaml = r#"
version: 1
name: stuck
channels:
  count: { schema: { type: integer } }
nodes:
  step: { kind: command, run: 'echo "{\"count\": 1}"', writes: [count], idempotent: true }
  again: { kind: loop, body: [step], until: 'channels.count >= 99', max_iterations: 10 }
"#;
    let report = run(&fixture, yaml, "l3", Caps::default()).await;
    assert_eq!(
        report.outcomes.get("again").map(String::as_str),
        Some("stalled")
    );
}

#[tokio::test]
async fn a_command_that_started_but_never_finished_is_reported_not_run_again() {
    let fixture = fixture(scripted(vec![]));
    let yaml = r#"
version: 1
name: effect
channels:
  out: { schema: { type: object } }
nodes:
  push: { kind: command, run: 'touch ran.txt', output: out }
"#;
    let workflow = load(yaml).unwrap();
    let shared = Shared::new(
        "e1".to_owned(),
        fixture.store.clone(),
        fixture.host.clone(),
        load(yaml).unwrap(),
    )
    .unwrap();
    fixture
        .store
        .append(NewEvent {
            session_id: nodes::journal_session("e1"),
            parent_id: None,
            kind: EventKind::NodeStarted,
            payload: json!({"node": "push", "ordinal": 0}),
        })
        .await
        .unwrap();
    fixture
        .store
        .create_workflow_run(jensen_harness_core::store::workflow::WorkflowRun {
            run_id: "e1".to_owned(),
            spec_hash: workflow.hash().to_owned(),
            spec: "{}".to_owned(),
            inputs: "{}".to_owned(),
        })
        .await
        .unwrap();
    let node = workflow.spec().nodes.get("push").unwrap();

    let finished = nodes::execute(&shared, "push", node, 0, &Values::new())
        .await
        .unwrap();

    assert_eq!(finished.outcome, "interrupted");
    assert!(!fixture.root.join("ran.txt").exists());
    let out: &Value = finished.outputs.get("out").unwrap();
    assert_eq!(out.get("exit_code"), Some(&json!(-1)));
}

#[derive(Debug)]
struct Never;

#[async_trait]
impl Provider for Never {
    fn id(&self) -> &str {
        "never"
    }

    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(ProviderError::Malformed(
            "no model call expected".to_owned(),
        ))
    }
}

#[tokio::test]
async fn a_workflow_without_agents_never_calls_a_model() {
    let fixture = fixture(Arc::new(Never));
    let report = run(
        &fixture,
        &counting("channels.count >= 1", 3),
        "n1",
        Caps::default(),
    )
    .await;
    assert_eq!(report.status, RunStatus::Succeeded);
}

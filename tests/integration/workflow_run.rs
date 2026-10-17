use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use dagrs::{MemoryRunStore, RunStore};
use jensen_harness_core::budget::{BudgetSettings, Caps};
use jensen_harness_core::providers::{
    CompletionRequest, CompletionResponse, Provider, ProviderError,
};
use jensen_harness_core::store::{EventKind, NewEvent, Store};
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_promote::Isolation;
use jensen_harness_workflow::nodes::{self, Values};
use jensen_harness_workflow::runtime::Shared;
use jensen_harness_workflow::{
    Host, RoleBinding, RunError, RunReport, RunStatus, Runner, StartRequest, load,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::common::{Scripted, calls_tool, path_input, says, tool_set};

struct TestHost {
    provider: Arc<dyn Provider>,
    dir: PathBuf,
    budget: BudgetSettings,
    isolation: Option<Isolation>,
}

impl Host for TestHost {
    fn bind(&self, _role: &str) -> Result<RoleBinding, RunError> {
        Ok(RoleBinding {
            provider: Arc::clone(&self.provider),
            model_address: "scripted/m".to_owned(),
        })
    }

    fn tools(&self, _access: Access, root: &std::path::Path) -> Result<ToolSet, RunError> {
        Ok(tool_set(root))
    }

    fn isolation(&self) -> Option<&Isolation> {
        self.isolation.as_ref()
    }

    fn worktree(&self) -> &std::path::Path {
        &self.dir
    }

    fn forge(
        &self,
        remote_url: &str,
    ) -> Result<jensen_harness_promote::forge::ForgeTarget, RunError> {
        Err(jensen_harness_promote::PromoteError::UnknownForge(remote_url.to_owned()).into())
    }

    fn extensions(&self) -> &jensen_harness_extensions::Extensions {
        static NONE: std::sync::OnceLock<jensen_harness_extensions::Extensions> =
            std::sync::OnceLock::new();
        NONE.get_or_init(jensen_harness_extensions::Extensions::default)
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
        isolation: None,
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
            worktree: "/w".into(),
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

#[derive(Debug)]
struct HangsAfter {
    inner: Scripted,
    answered: usize,
    calls: AtomicUsize,
}

#[async_trait]
impl Provider for HangsAfter {
    fn id(&self) -> &str {
        "scripted"
    }

    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call >= self.answered {
            std::future::pending::<()>().await;
        }
        self.inner.complete(request).await
    }
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let waited = std::time::Instant::now();
    while !condition() {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(waited.elapsed() < Duration::from_secs(10), "{what}");
    }
}

const READER: &str = r#"
version: 1
name: reader
channels:
  findings: { schema: { type: array }, reducer: append }
nodes:
  scout: { kind: agent, role: r, prompt: look, writes: [findings], max_turns: 5 }
"#;

fn file_runner(fixture: &Fixture, checkpoints: &std::path::Path) -> Runner {
    Runner {
        store: fixture.store.clone(),
        host: fixture.host.clone(),
        checkpoints: Arc::new(dagrs::FileRunStore::new(checkpoints).unwrap()),
    }
}

#[tokio::test]
async fn an_agent_killed_mid_call_resumes_its_own_journal_without_repeating_a_completed_call() {
    let checkpoints = tempfile::tempdir().unwrap();
    let hanging = Arc::new(HangsAfter {
        inner: Scripted::of(vec![calls_tool("t1", "read", path_input("a.txt"))]),
        answered: 1,
        calls: AtomicUsize::new(0),
    });
    let first = fixture(hanging.clone());
    std::fs::write(first.root.join("a.txt"), "alpha").unwrap();
    let running = {
        let runner = file_runner(&first, checkpoints.path());
        tokio::spawn(async move {
            runner
                .start(
                    load(READER).unwrap(),
                    StartRequest {
                        run_id: "k1".to_owned(),
                        inputs: Values::new(),
                        caps: Caps::default(),
                    },
                    CancellationToken::new(),
                )
                .await
        })
    };
    wait_until("the second model call never started", || {
        hanging.calls.load(Ordering::SeqCst) >= 2
    })
    .await;
    running.abort();
    let _ = running.await;

    let resumed_provider = Arc::new(Scripted::of(vec![says(r#"{"findings": ["alpha"]}"#)]));
    let second = Fixture {
        _dir: tempfile::tempdir().unwrap(),
        root: first.root.clone(),
        store: first.store.clone(),
        host: Arc::new(TestHost {
            provider: resumed_provider.clone(),
            dir: first.root.clone(),
            budget: BudgetSettings::default(),
            isolation: None,
        }),
    };
    let report = file_runner(&second, checkpoints.path())
        .resume(load(READER).unwrap(), "k1", CancellationToken::new())
        .await
        .unwrap();

    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(report.channels.get("findings"), Some(&json!(["alpha"])));
    assert_eq!(
        resumed_provider.calls(),
        1,
        "only the lost call is made again"
    );
    let sent = format!("{:?}", resumed_provider.request(0).turns);
    assert!(
        sent.contains("alpha"),
        "the finished tool result is replayed"
    );
    let chain = first.store.usage("k1:scout:0").await.unwrap();
    assert!(
        first
            .store
            .open_reservations_under("k1")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(chain.iter().any(|u| u.spent_tokens > 0));
}

const CHAIN: &str = r#"
version: 1
name: chain
channels:
  one: { schema: { type: object } }
  two: { schema: { type: object } }
nodes:
  first: { kind: command, run: 'echo x >> first.log', output: one }
  second:
    kind: command
    run: 'touch second.started; while [ ! -f go ]; do sleep 0.05; done; echo y'
    reads: [one]
    output: two
    idempotent: true
edges:
  - { from: first, to: second }
"#;

#[tokio::test]
async fn a_run_dropped_between_waves_resumes_without_rerunning_finished_nodes() {
    let checkpoints = tempfile::tempdir().unwrap();
    let first = fixture(scripted(vec![]));
    let running = {
        let runner = file_runner(&first, checkpoints.path());
        tokio::spawn(async move {
            runner
                .start(
                    load(CHAIN).unwrap(),
                    StartRequest {
                        run_id: "c1".to_owned(),
                        inputs: Values::new(),
                        caps: Caps::default(),
                    },
                    CancellationToken::new(),
                )
                .await
        })
    };
    let marker = first.root.join("second.started");
    wait_until("the second node never started", || marker.exists()).await;
    running.abort();
    let _ = running.await;
    std::fs::write(first.root.join("go"), "").unwrap();

    let report = file_runner(&first, checkpoints.path())
        .resume(load(CHAIN).unwrap(), "c1", CancellationToken::new())
        .await
        .unwrap();

    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(
        std::fs::read_to_string(first.root.join("first.log")).unwrap(),
        "x\n",
        "the finished node ran once"
    );
    assert_eq!(
        report.channels.get("two").unwrap().get("stdout"),
        Some(&json!("y\n"))
    );
    assert_eq!(
        report.channels.get("one").unwrap().get("stdout"),
        Some(&json!(""))
    );
}

#[tokio::test]
async fn a_finished_run_is_reported_not_run_again() {
    let checkpoints = tempfile::tempdir().unwrap();
    let fixture = fixture(scripted(vec![]));
    let yaml = counting("channels.count >= 1", 3);
    let runner = file_runner(&fixture, checkpoints.path());
    let first = runner
        .start(
            load(&yaml).unwrap(),
            StartRequest {
                run_id: "d1".to_owned(),
                inputs: Values::new(),
                caps: Caps::default(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let again = runner
        .resume(load(&yaml).unwrap(), "d1", CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(again.status, RunStatus::Succeeded);
    assert_eq!(again.channels, first.channels);
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("c"))
            .unwrap()
            .trim(),
        "1"
    );
}

#[tokio::test]
async fn a_changed_spec_is_refused_and_an_unknown_run_is_unknown() {
    let checkpoints = tempfile::tempdir().unwrap();
    let fixture = fixture(scripted(vec![]));
    let runner = file_runner(&fixture, checkpoints.path());
    let yaml = counting("channels.count >= 1", 3);
    runner
        .start(
            load(&yaml).unwrap(),
            StartRequest {
                run_id: "s1".to_owned(),
                inputs: Values::new(),
                caps: Caps::default(),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let changed = counting("channels.count >= 2", 3);
    let refused = runner
        .resume(load(&changed).unwrap(), "s1", CancellationToken::new())
        .await;
    assert!(matches!(refused, Err(RunError::SpecChanged { .. })));
    let unknown = runner
        .resume(load(&yaml).unwrap(), "nope", CancellationToken::new())
        .await;
    assert!(matches!(unknown, Err(RunError::UnknownRun(_))));
}

#[tokio::test]
async fn cancelling_stops_the_nodes_and_leaves_no_held_reservation() {
    let hanging = Arc::new(HangsAfter {
        inner: Scripted::of(vec![]),
        answered: 0,
        calls: AtomicUsize::new(0),
    });
    let fixture = fixture(hanging.clone());
    let cancel = CancellationToken::new();
    let running = {
        let runner = runner(&fixture);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            runner
                .start(
                    load(READER).unwrap(),
                    StartRequest {
                        run_id: "x1".to_owned(),
                        inputs: Values::new(),
                        caps: Caps::default(),
                    },
                    cancel,
                )
                .await
        })
    };
    wait_until("the model call never started", || {
        hanging.calls.load(Ordering::SeqCst) >= 1
    })
    .await;
    assert!(
        !fixture
            .store
            .open_reservations_under("x1")
            .await
            .unwrap()
            .is_empty()
    );

    cancel.cancel();
    let report = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("a cancelled run ends promptly")
        .unwrap()
        .unwrap();

    assert_eq!(report.status, RunStatus::Cancelled);
    assert!(
        fixture
            .store
            .open_reservations_under("x1")
            .await
            .unwrap()
            .is_empty()
    );
    let events = fixture.store.events("wf:x1").await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, EventKind::WorkflowFinished);
    assert_eq!(last.payload.get("released_reservations"), Some(&json!(1)));
}

fn diamond(first_delay: &str, second_delay: &str) -> String {
    format!(
        r#"
version: 1
name: diamond
channels:
  tags: {{ schema: {{ type: array }}, reducer: set_union }}
  log: {{ schema: {{ type: array }}, reducer: append }}
  best: {{ schema: {{ type: number }}, reducer: max }}
  meta: {{ schema: {{ type: object }}, reducer: merge_map }}
  seen: {{ schema: {{ type: object }} }}
nodes:
  root: {{ kind: command, run: 'true' }}
  left:
    kind: command
    run: 'sleep {first_delay}; echo "{{\"tags\":[\"x\",\"y\"],\"log\":[\"left\"],\"best\":4,\"meta\":{{\"k\":\"left\",\"l\":1}}}}"'
    writes: [tags, log, best, meta]
  right:
    kind: command
    run: 'sleep {second_delay}; echo "{{\"tags\":[\"y\",\"z\"],\"log\":[\"right\"],\"best\":9,\"meta\":{{\"k\":\"right\"}}}}"'
    writes: [tags, log, best, meta]
  join: {{ kind: command, run: 'true', reads: [tags, log, best, meta], output: seen }}
edges:
  - {{ from: root, to: left }}
  - {{ from: root, to: right }}
  - {{ from: left, to: join }}
  - {{ from: right, to: join }}
"#
    )
}

#[tokio::test]
async fn the_merged_state_never_depends_on_which_branch_finishes_first() {
    let mut results = Vec::new();
    for (index, (first, second)) in [("0", "0.4"), ("0.4", "0"), ("0.1", "0.1")]
        .into_iter()
        .enumerate()
    {
        let fixture = fixture(scripted(vec![]));
        let report = run(
            &fixture,
            &diamond(first, second),
            &format!("dm{index}"),
            Caps::default(),
        )
        .await;
        assert!(report.failure.is_none(), "{:?}", report.failure);
        results.push((
            report.channels.get("tags").cloned(),
            report.channels.get("log").cloned(),
            report.channels.get("best").cloned(),
            report.channels.get("meta").cloned(),
        ));
    }
    assert!(
        results.windows(2).all(|pair| pair[0] == pair[1]),
        "{results:?}"
    );
    let (tags, log, best, meta) = results.remove(0);
    assert_eq!(tags, Some(json!(["x", "y", "z"])));
    assert_eq!(log, Some(json!(["left", "right"])));
    assert_eq!(best, Some(json!(9)));
    assert_eq!(meta, Some(json!({"k": "right", "l": 1})));
}

#[tokio::test]
async fn an_agent_session_created_but_never_prompted_is_prompted_on_resume() {
    use jensen_harness_core::agent::{Session, SessionConfig};
    use jensen_harness_core::budget::BudgetSettings;
    use jensen_harness_core::store::{NewScope, ScopeKind};

    let fixture = fixture(scripted(vec![says(r#"{"findings": ["a"]}"#)]));
    for (id, parent, kind) in [
        ("g1", None, ScopeKind::Run),
        ("g1:scout:0", Some("g1".to_owned()), ScopeKind::Node),
    ] {
        fixture
            .store
            .open_scope(NewScope {
                id: id.to_owned(),
                parent_id: parent,
                kind,
                caps: Caps::default(),
                soft_percent: 80,
            })
            .await
            .unwrap();
    }
    Session::create(
        fixture.store.clone(),
        SessionConfig {
            session_id: "g1.scout.0".to_owned(),
            run_id: Some("g1:scout:0".to_owned()),
            model_address: "scripted/m".to_owned(),
            max_turns: 3,
            budget: BudgetSettings::default()
                .resolve(None, "scripted/m")
                .unwrap(),
            price_version: String::new(),
        },
        tool_set(&fixture.root),
    )
    .await
    .unwrap();

    let report = run(&fixture, READER, "g1", Caps::default()).await;

    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(report.channels.get("findings"), Some(&json!(["a"])));
}

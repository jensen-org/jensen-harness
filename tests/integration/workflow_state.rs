use jensen_harness_core::budget::Caps;
use jensen_harness_core::store::workflow::WorkflowRun;
use jensen_harness_core::store::{NewScope, ReserveOutcome, ReserveRequest, ScopeKind, Store};
use jensen_harness_workflow::reducer::apply;
use jensen_harness_workflow::spec::Reducer;
use jensen_harness_workflow::state::{State, StateError, externalize, resolve};
use jensen_harness_workflow::{Workflow, load};
use proptest::prelude::*;
use serde_json::{Value, json};

const DIAMOND: &str = "
version: 1
name: diamond
channels:
  seed: { schema: { type: string }, init: s }
  tags: { schema: { type: array }, reducer: set_union }
  log: { schema: { type: array }, reducer: append }
  best: { schema: { type: number }, reducer: max }
  meta: { schema: { type: object }, reducer: merge_map }
  note: { schema: { type: string } }
nodes:
  a: { kind: command, run: x, reads: [seed], writes: [note] }
  b: { kind: command, run: x, reads: [seed], writes: [tags, log, best, meta] }
  c: { kind: command, run: x, reads: [seed], writes: [tags, log, best, meta] }
  d: { kind: command, run: x, reads: [tags, log, best, meta, note] }
edges:
  - { from: a, to: b }
  - { from: a, to: c }
  - { from: b, to: d }
  - { from: c, to: d }
";

fn diamond() -> Workflow {
    load(DIAMOND).unwrap()
}

#[test]
fn every_reducer_combines_as_documented() {
    let held = |v: Value| Some(v);
    assert_eq!(
        apply(Reducer::Overwrite, held(json!(1)), json!(2)).unwrap(),
        json!(2)
    );
    assert_eq!(
        apply(Reducer::Append, held(json!([1])), json!([2, 1])).unwrap(),
        json!([1, 2, 1])
    );
    assert_eq!(
        apply(Reducer::SetUnion, held(json!([1, 2])), json!([2, 3])).unwrap(),
        json!([1, 2, 3])
    );
    assert_eq!(
        apply(
            Reducer::MergeMap,
            held(json!({"a": 1, "b": 1})),
            json!({"b": 2})
        )
        .unwrap(),
        json!({"a": 1, "b": 2})
    );
    assert_eq!(
        apply(Reducer::Max, held(json!(3)), json!(2)).unwrap(),
        json!(3)
    );
    assert_eq!(
        apply(Reducer::Max, held(json!(1.5)), json!(2)).unwrap(),
        json!(2)
    );
    assert_eq!(
        apply(Reducer::Append, None, json!([1])).unwrap(),
        json!([1])
    );
    assert!(apply(Reducer::Append, held(json!([1])), json!("x")).is_err());
    assert!(apply(Reducer::MergeMap, held(json!({})), json!([1])).is_err());
}

fn fold(order: &[usize]) -> Vec<Value> {
    let workflow = diamond();
    let mut state = State::new(&workflow).unwrap();
    let writes: Vec<(&str, &str, Value)> = vec![
        ("b", "tags", json!(["x", "y"])),
        ("c", "tags", json!(["y", "z"])),
        ("b", "log", json!(["from b"])),
        ("c", "log", json!(["from c"])),
        ("b", "best", json!(4)),
        ("c", "best", json!(9)),
        ("b", "meta", json!({"k": "b", "only_b": 1})),
        ("c", "meta", json!({"k": "c"})),
        ("a", "note", json!("hello")),
    ];
    for index in order {
        let (node, channel, value) = writes.get(*index).unwrap().clone();
        state.write(node, channel, value).unwrap();
    }
    ["tags", "log", "best", "meta", "note"]
        .iter()
        .map(|channel| state.value(channel).unwrap().unwrap())
        .collect()
}

proptest! {
    #[test]
    fn the_folded_state_never_depends_on_completion_order(
        order in Just((0..9usize).collect::<Vec<_>>()).prop_shuffle()
    ) {
        let reference = fold(&(0..9).collect::<Vec<_>>());
        prop_assert_eq!(fold(&order), reference);
    }
}

#[test]
fn a_write_against_the_schema_or_outside_the_declared_set_is_refused() {
    let workflow = diamond();
    let mut state = State::new(&workflow).unwrap();
    assert!(matches!(
        state.write("a", "note", json!(7)),
        Err(StateError::SchemaViolation { channel, .. }) if channel == "note"
    ));
    assert!(matches!(
        state.write("a", "tags", json!([])),
        Err(StateError::UndeclaredWrite { .. })
    ));
    assert!(matches!(
        state.write("a", "ghost", json!([])),
        Err(StateError::UnknownChannel(_))
    ));
}

#[test]
fn init_is_the_base_and_memo_keys_follow_inputs_and_definition() {
    let workflow = diamond();
    let mut state = State::new(&workflow).unwrap();
    assert_eq!(state.value("seed").unwrap(), Some(json!("s")));
    let node = workflow.spec().nodes.get("d").unwrap();
    let reads = node.reads().to_vec();
    state.write("b", "best", json!(1)).unwrap();
    state.write("a", "note", json!("n")).unwrap();
    let before = state.memo_key(node, &reads).unwrap();
    assert_eq!(before, state.memo_key(node, &reads).unwrap());
    state.write("c", "best", json!(5)).unwrap();
    assert_ne!(before, state.memo_key(node, &reads).unwrap());
    let other = workflow.spec().nodes.get("b").unwrap();
    assert_ne!(
        state.memo_key(node, &reads).unwrap(),
        state.memo_key(other, &reads).unwrap()
    );
}

#[tokio::test]
async fn large_values_become_artifacts_and_resolve_back_intact() {
    let store = Store::in_memory().unwrap();
    let small = json!({"a": 1});
    assert_eq!(externalize(&store, small.clone()).await.unwrap(), small);
    let large = json!({"blob": "x".repeat(40_000)});
    let reference = externalize(&store, large.clone()).await.unwrap();
    assert!(reference.get("$artifact").is_some());
    assert_eq!(resolve(&store, reference).await.unwrap(), large);
    assert!(matches!(
        resolve(&store, json!({"$artifact": "0".repeat(64)})).await,
        Err(StateError::MissingArtifact(_))
    ));
}

#[tokio::test]
async fn memoized_results_and_runs_persist_and_are_append_only() {
    let store = Store::in_memory().unwrap();
    store
        .create_workflow_run(WorkflowRun {
            run_id: "r1".into(),
            spec_hash: "h".into(),
            spec: "{}".into(),
            inputs: "{}".into(),
            worktree: "/w".into(),
        })
        .await
        .unwrap();
    assert_eq!(store.memoized_result("k").await.unwrap(), None);
    store
        .record_node_result("r1", "n", "k", "{\"a\":1}")
        .await
        .unwrap();
    store
        .record_node_result("r1", "n", "k", "{\"a\":2}")
        .await
        .unwrap();
    assert_eq!(
        store.memoized_result("k").await.unwrap().unwrap(),
        "{\"a\":2}"
    );
    assert_eq!(
        store.workflow_run("r1").await.unwrap().unwrap().spec_hash,
        "h"
    );
    assert!(
        store
            .record_node_result("ghost", "n", "k", "{}")
            .await
            .is_err()
    );
    assert!(
        store
            .create_workflow_run(WorkflowRun {
                run_id: "r1".into(),
                spec_hash: "again".into(),
                spec: "{}".into(),
                inputs: "{}".into(),
                worktree: "/w".into(),
            })
            .await
            .is_err()
    );
}

const V1_SCHEMA: &str = "
CREATE TABLE events (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE, parent_id TEXT,
    session_id TEXT NOT NULL, kind TEXT NOT NULL, payload TEXT NOT NULL, created_at_ms INTEGER NOT NULL);
CREATE TABLE scopes (id TEXT PRIMARY KEY, parent_id TEXT REFERENCES scopes (id),
    kind TEXT NOT NULL CHECK (kind IN ('run', 'session')),
    max_tokens INTEGER CHECK (max_tokens IS NULL OR max_tokens > 0),
    max_cost_micro_usd INTEGER CHECK (max_cost_micro_usd IS NULL OR max_cost_micro_usd > 0),
    soft_percent INTEGER NOT NULL CHECK (soft_percent BETWEEN 1 AND 99), created_at_ms INTEGER NOT NULL);
CREATE TABLE ledger (seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT NOT NULL UNIQUE,
    scope_id TEXT NOT NULL REFERENCES scopes (id),
    kind TEXT NOT NULL CHECK (kind IN ('reserve', 'settle', 'release', 'adjust')),
    call_id TEXT NOT NULL, tokens INTEGER NOT NULL, cost_micro_usd INTEGER NOT NULL,
    expires_at_ms INTEGER, created_at_ms INTEGER NOT NULL);
CREATE UNIQUE INDEX ledger_one_reserve_per_call ON ledger (call_id) WHERE kind = 'reserve';
CREATE UNIQUE INDEX ledger_one_end_per_call ON ledger (call_id) WHERE kind IN ('settle', 'release');
CREATE TRIGGER scopes_no_update BEFORE UPDATE ON scopes
BEGIN SELECT RAISE(ABORT, 'scopes are append only'); END;
CREATE TRIGGER scopes_no_delete BEFORE DELETE ON scopes
BEGIN SELECT RAISE(ABORT, 'scopes are append only'); END;
INSERT INTO scopes VALUES ('run1', NULL, 'run', 1000, NULL, 80, 1);
INSERT INTO scopes VALUES ('s1', 'run1', 'session', NULL, NULL, 80, 2);
INSERT INTO ledger (id, scope_id, kind, call_id, tokens, cost_micro_usd, created_at_ms)
    VALUES ('l1', 's1', 'reserve', 'c1', 100, 0, 3);
INSERT INTO ledger (id, scope_id, kind, call_id, tokens, cost_micro_usd, created_at_ms)
    VALUES ('l2', 's1', 'settle', 'c1', 60, 0, 4);
PRAGMA user_version = 1;
";

#[tokio::test]
async fn a_populated_v1_database_migrates_with_its_history_intact() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(V1_SCHEMA).unwrap();
    }
    let store = Store::open(&path).unwrap();
    let usage = store.usage("s1").await.unwrap();
    assert_eq!(
        usage
            .iter()
            .find(|u| u.scope_id == "run1")
            .unwrap()
            .spent_tokens,
        60
    );
    store
        .open_scope(NewScope {
            id: "n1".into(),
            parent_id: Some("run1".into()),
            kind: ScopeKind::Node,
            caps: Caps {
                max_tokens: Some(500),
                max_cost_micro_usd: None,
            },
            soft_percent: 80,
        })
        .await
        .unwrap();
    let outcome = store
        .reserve(ReserveRequest {
            scope_id: "n1".into(),
            call_id: "c2".into(),
            tokens: 100,
            cost_micro_usd: 0,
            ttl_ms: 60_000,
        })
        .await
        .unwrap();
    assert!(matches!(outcome, ReserveOutcome::Granted(_)));
    let run = store.usage("n1").await.unwrap();
    assert_eq!(
        run.iter()
            .find(|u| u.scope_id == "run1")
            .unwrap()
            .held_tokens,
        100
    );
    drop(store);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert!(conn.execute("DELETE FROM scopes", []).is_err());
}

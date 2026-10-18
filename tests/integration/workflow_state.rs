use jensen_harness_core::budget::Caps;
use jensen_harness_core::store::workflow::WorkflowRun;
use jensen_harness_core::store::{NewScope, ReserveOutcome, ReserveRequest, ScopeKind, Store};
use jensen_harness_workflow::reducer::apply;
use jensen_harness_workflow::spec::Reducer;
use jensen_harness_workflow::state::{StateError, externalize, resolve};
use proptest::prelude::*;
use serde_json::{Value, json};

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

fn union_of(batches: &[Vec<u8>]) -> Vec<u64> {
    let mut held: Option<Value> = None;
    for batch in batches {
        held = Some(apply(Reducer::SetUnion, held, json!(batch)).unwrap());
    }
    let mut members: Vec<u64> = held
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .map(|member| member.as_u64().unwrap())
        .collect();
    members.sort_unstable();
    members
}

fn best_of(batches: &[Vec<u8>]) -> Value {
    let mut held: Option<Value> = None;
    for batch in batches {
        let top = batch.iter().max().copied().unwrap_or(0);
        held = Some(apply(Reducer::Max, held, json!(top)).unwrap());
    }
    held.unwrap()
}

proptest! {
    #[test]
    fn set_union_and_max_give_the_same_answer_in_any_order(
        pair in proptest::collection::vec(proptest::collection::vec(0u8..8, 0..5), 1..6)
            .prop_flat_map(|batches| (Just(batches.clone()), Just(batches).prop_shuffle()))
    ) {
        let (batches, shuffled) = pair;
        prop_assert_eq!(union_of(&batches), union_of(&shuffled));
        prop_assert_eq!(best_of(&batches), best_of(&shuffled));
    }
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

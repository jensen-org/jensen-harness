use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use dagrs::{
    DagrsResult, FileRunStore, HarnessRun, Meter, ParticipantId, ParticipantRegistry, ReducerKind,
    RunConfig, RunDependencies, RunStatus, RunStore, State, StateDelta, StateKey, StateSchema,
    StateSnapshot, StateStepExecutor, StepOutcome, StepParticipant, StepTransition, WorkResults,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

type Behavior =
    dyn Fn(&StateSnapshot, &Keys) -> DagrsResult<(StateDelta, StepTransition)> + Send + Sync;

#[derive(Clone)]
struct Keys(Arc<BTreeMap<String, StateKey<Value>>>);

impl Keys {
    fn get(&self, name: &str) -> &StateKey<Value> {
        self.0.get(name).unwrap()
    }
}

fn schema_with(channels: &[(&str, ReducerKind)]) -> (StateSchema, Keys) {
    let mut schema = StateSchema::new(1).unwrap();
    schema
        .register_reducer("set_union", |old: &Value, new: &Value| {
            let mut all: BTreeSet<String> = BTreeSet::new();
            for side in [old, new] {
                for item in side.as_array().into_iter().flatten() {
                    all.insert(item.as_str().unwrap().to_string());
                }
            }
            Ok(json!(all.into_iter().collect::<Vec<_>>()))
        })
        .unwrap();
    schema
        .register_reducer("max", |old: &Value, new: &Value| {
            Ok(if new.as_i64() > old.as_i64() {
                new.clone()
            } else {
                old.clone()
            })
        })
        .unwrap();
    let mut keys = BTreeMap::new();
    for (name, reducer) in channels {
        keys.insert(
            (*name).to_string(),
            schema
                .register_key::<Value>(*name, 1, reducer.clone())
                .unwrap(),
        );
    }
    (schema, Keys(Arc::new(keys)))
}

struct Node {
    id: ParticipantId,
    reads: Vec<String>,
    writes: Vec<String>,
    delay: Duration,
    runs: Arc<AtomicUsize>,
    keys: Keys,
    behavior: Arc<Behavior>,
}

impl Node {
    fn new(
        id: &str,
        reads: &[&str],
        writes: &[&str],
        keys: &Keys,
        behavior: impl Fn(&StateSnapshot, &Keys) -> DagrsResult<(StateDelta, StepTransition)>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            id: ParticipantId::new(id).unwrap(),
            reads: reads.iter().map(|s| (*s).to_string()).collect(),
            writes: writes.iter().map(|s| (*s).to_string()).collect(),
            delay: Duration::ZERO,
            runs: Arc::new(AtomicUsize::new(0)),
            keys: keys.clone(),
            behavior: Arc::new(behavior),
        }
    }

    fn delayed(mut self, millis: u64) -> Self {
        self.delay = Duration::from_millis(millis);
        self
    }

    fn counting(mut self, runs: &Arc<AtomicUsize>) -> Self {
        self.runs = Arc::clone(runs);
        self
    }
}

#[async_trait]
impl StepParticipant for Node {
    fn id(&self) -> ParticipantId {
        self.id.clone()
    }

    fn read_keys(&self) -> BTreeSet<String> {
        self.reads.iter().cloned().collect()
    }

    fn write_keys(&self) -> BTreeSet<String> {
        self.writes.iter().cloned().collect()
    }

    async fn run(&self, snapshot: StateSnapshot, _work: WorkResults) -> DagrsResult<StepOutcome> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        let (delta, next) = (self.behavior)(&snapshot, &self.keys)?;
        Ok(StepOutcome::new(delta, Vec::new(), next))
    }
}

fn pid(name: &str) -> ParticipantId {
    ParticipantId::new(name).unwrap()
}

fn write(keys: &Keys, name: &str, value: Value) -> StateDelta {
    let mut delta = StateDelta::new();
    delta.set(keys.get(name), value).unwrap();
    delta
}

fn then(ids: &[&str]) -> StepTransition {
    StepTransition::continue_with(ids.iter().map(|id| pid(id)))
}

fn finish() -> StepTransition {
    StepTransition::finish()
}

async fn final_state(store: &Arc<dyn RunStore>, thread_id: &str) -> (State, StateSchema) {
    let runs = store.list_runs(thread_id, None, 10).await.unwrap();
    let run = runs.runs().first().unwrap().clone();
    let checkpoint = store
        .load_latest_checkpoint_for_run(run.run_id())
        .await
        .unwrap();
    let schema = StateSchema::from_definition(checkpoint.state_schema().clone()).unwrap();
    (checkpoint.state().clone(), schema)
}

#[tokio::test]
async fn json_channels_with_custom_reducers_merge_the_same_whatever_finishes_first() {
    let channels = [
        ("tags", ReducerKind::Custom("set_union".to_string())),
        ("best", ReducerKind::Custom("max".to_string())),
        ("log", ReducerKind::Append),
    ];
    let mut results = Vec::new();
    for (a_delay, b_delay) in [(0, 40), (40, 0)] {
        let (schema, keys) = schema_with(&channels);
        let mut executor = StateStepExecutor::new(schema.clone(), schema.empty_state()).unwrap();
        for (id, delay, tag, score) in [("a", a_delay, "alpha", 3), ("b", b_delay, "beta", 9)] {
            executor
                .register(
                    Node::new(id, &[], &["tags", "best", "log"], &keys, move |_, k| {
                        let mut delta = write(k, "tags", json!([tag]));
                        delta.set(k.get("best"), json!(score)).unwrap();
                        delta.set(k.get("log"), json!([tag])).unwrap();
                        Ok((delta, then(&["done"])))
                    })
                    .delayed(delay),
                )
                .unwrap();
        }
        executor
            .register(Node::new("done", &[], &[], &keys, |_, _| {
                Ok((StateDelta::new(), finish()))
            }))
            .unwrap();
        let step = executor
            .execute_step([pid("a"), pid("b")], WorkResults::default())
            .await
            .unwrap();
        results.push((
            step.state.get(&schema, keys.get("tags")).unwrap(),
            step.state.get(&schema, keys.get("best")).unwrap(),
            step.state.get(&schema, keys.get("log")).unwrap(),
        ));
    }
    assert_eq!(
        results[0], results[1],
        "completion order must not change the merge"
    );
    assert_eq!(results[0].0, Some(json!(["alpha", "beta"])));
    assert_eq!(results[0].1, Some(json!(9)));
    assert_eq!(results[0].2, Some(json!(["alpha", "beta"])));
}

#[tokio::test]
async fn a_write_outside_the_declared_set_fails_the_step_and_commits_nothing() {
    let (schema, keys) = schema_with(&[
        ("allowed", ReducerKind::Overwrite),
        ("secret", ReducerKind::Overwrite),
    ]);
    let mut executor = StateStepExecutor::new(schema.clone(), schema.empty_state()).unwrap();
    executor
        .register(Node::new("sneaky", &[], &["allowed"], &keys, |_, k| {
            let mut delta = write(k, "allowed", json!(1));
            delta.set(k.get("secret"), json!(2)).unwrap();
            Ok((delta, finish()))
        }))
        .unwrap();

    let error = executor
        .execute_step([pid("sneaky")], WorkResults::default())
        .await
        .unwrap_err();

    assert!(format!("{error:?}").contains("DgStt0006"), "{error:?}");
    assert_eq!(
        executor.state().revision(),
        0,
        "partial deltas are not committed"
    );
}

#[tokio::test]
async fn two_writers_to_an_overwrite_channel_in_one_step_conflict() {
    let (schema, keys) = schema_with(&[("answer", ReducerKind::Overwrite)]);
    let mut executor = StateStepExecutor::new(schema.clone(), schema.empty_state()).unwrap();
    for id in ["x", "y"] {
        executor
            .register(Node::new(id, &[], &["answer"], &keys, |_, k| {
                Ok((write(k, "answer", json!(1)), then(&["x"])))
            }))
            .unwrap();
    }
    assert!(
        executor
            .execute_step([pid("x"), pid("y")], WorkResults::default())
            .await
            .is_err()
    );
}

fn dependencies(
    registry: ParticipantRegistry,
    store: Arc<dyn RunStore>,
    schema: StateSchema,
) -> RunDependencies {
    let initial = schema.empty_state();
    RunDependencies::new(registry, store, schema, initial).unwrap()
}

#[tokio::test]
async fn parallel_participants_overlap_and_the_next_wave_sees_their_merged_writes() {
    let dir = tempfile::tempdir().unwrap();
    let store: Arc<dyn RunStore> = Arc::new(FileRunStore::new(dir.path()).unwrap());
    let (schema, keys) = schema_with(&[
        ("notes", ReducerKind::Append),
        ("seen", ReducerKind::Overwrite),
    ]);
    let registry = ParticipantRegistry::new();
    for id in ["a", "b", "c"] {
        registry
            .register(
                Node::new(id, &[], &["notes"], &keys, move |_, k| {
                    Ok((write(k, "notes", json!([id])), then(&["join"])))
                })
                .delayed(150),
            )
            .unwrap();
    }
    registry
        .register(Node::new(
            "join",
            &["notes"],
            &["seen"],
            &keys,
            |snapshot, k| {
                let notes = snapshot.get(k.get("notes"))?.unwrap();
                Ok((write(k, "seen", notes), finish()))
            },
        ))
        .unwrap();

    let started = Instant::now();
    let report = HarnessRun::start(
        RunConfig::new().with_thread_id("overlap"),
        [pid("a"), pid("b"), pid("c")],
        dependencies(registry, Arc::clone(&store), schema),
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::Succeeded);
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "three 150ms nodes must overlap"
    );
    assert_eq!(report.super_steps_completed, 2);
    let (state, mut schema) = final_state(&store, "overlap").await;
    let seen_key = schema.bind_key::<Value>("seen", 1).unwrap();
    let seen = state.get(&schema, &seen_key).unwrap();
    assert_eq!(seen, Some(json!(["a", "b", "c"])));
}

#[tokio::test]
async fn a_run_killed_mid_wave_resumes_from_its_last_commit_without_rerunning_finished_work() {
    let dir = tempfile::tempdir().unwrap();
    let channels = [("trail", ReducerKind::Append)];
    let first_runs = Arc::new(AtomicUsize::new(0));
    let second_runs = Arc::new(AtomicUsize::new(0));
    let blocked_runs = Arc::new(AtomicUsize::new(0));

    let build = |store: Arc<dyn RunStore>,
                 first: &Arc<AtomicUsize>,
                 second: &Arc<AtomicUsize>,
                 block: bool| {
        let (schema, keys) = schema_with(&channels);
        let registry = ParticipantRegistry::new();
        registry
            .register(
                Node::new("first", &[], &["trail"], &keys, |_, k| {
                    Ok((write(k, "trail", json!(["first"])), then(&["second"])))
                })
                .counting(first),
            )
            .unwrap();
        let second_node = Node::new("second", &["trail"], &["trail"], &keys, |_, k| {
            Ok((write(k, "trail", json!(["second"])), then(&["last"])))
        })
        .counting(second);
        registry
            .register(if block {
                second_node.delayed(60_000)
            } else {
                second_node
            })
            .unwrap();
        registry
            .register(Node::new("last", &["trail"], &[], &keys, |_, _| {
                Ok((StateDelta::new(), finish()))
            }))
            .unwrap();
        (dependencies(registry, store, schema.clone()), schema)
    };

    let store: Arc<dyn RunStore> = Arc::new(FileRunStore::new(dir.path()).unwrap());
    let (deps, _) = build(Arc::clone(&store), &first_runs, &blocked_runs, true);
    let running = tokio::spawn(HarnessRun::start(
        RunConfig::new().with_thread_id("crash"),
        [pid("first")],
        deps,
    ));
    let waited = Instant::now();
    while blocked_runs.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "the second wave never started"
        );
    }
    running.abort();
    let _ = running.await;
    drop(store);

    let store: Arc<dyn RunStore> = Arc::new(FileRunStore::new(dir.path()).unwrap());
    let runs = store.list_runs("crash", None, 10).await.unwrap();
    let latest = runs
        .runs()
        .first()
        .unwrap()
        .latest_checkpoint_id()
        .to_string();
    let (deps, _) = build(
        Arc::clone(&store),
        &Arc::new(AtomicUsize::new(0)),
        &second_runs,
        false,
    );
    let report = HarnessRun::resume("crash", &latest, deps).await.unwrap();

    assert_eq!(report.status, RunStatus::Succeeded);
    assert_eq!(
        first_runs.load(Ordering::SeqCst),
        1,
        "the first wave ran once, before the crash"
    );
    assert_eq!(
        second_runs.load(Ordering::SeqCst),
        1,
        "only the interrupted wave ran again"
    );
    let (state, mut schema) = final_state(&store, "crash").await;
    let trail_key = schema.bind_key::<Value>("trail", 1).unwrap();
    let trail = state.get(&schema, &trail_key).unwrap();
    assert_eq!(trail, Some(json!(["first", "second"])));
}

#[tokio::test]
async fn a_cycle_is_cut_off_by_the_step_bound_with_a_typed_cause() {
    let store: Arc<dyn RunStore> = Arc::new(dagrs::MemoryRunStore::new());
    let (schema, keys) = schema_with(&[("n", ReducerKind::Append)]);
    let registry = ParticipantRegistry::new();
    registry
        .register(Node::new("tick", &[], &["n"], &keys, |_, k| {
            Ok((write(k, "n", json!([1])), then(&["tick"])))
        }))
        .unwrap();

    let report = HarnessRun::start(
        RunConfig::new().with_max_super_steps(5),
        [pid("tick")],
        dependencies(registry, store, schema),
    )
    .await
    .unwrap();

    assert_ne!(report.status, RunStatus::Succeeded);
    assert_eq!(report.super_steps_completed, 5);
    assert!(report.terminal_cause.is_some());
}

#[tokio::test]
async fn cancelling_stops_the_run_with_a_cancelled_status() {
    let store: Arc<dyn RunStore> = Arc::new(dagrs::MemoryRunStore::new());
    let (schema, keys) = schema_with(&[("n", ReducerKind::Append)]);
    let registry = ParticipantRegistry::new();
    registry
        .register(
            Node::new("slow", &[], &[], &keys, |_, _| {
                Ok((StateDelta::new(), finish()))
            })
            .delayed(60_000),
        )
        .unwrap();
    let token = CancellationToken::new();
    let deps = dependencies(registry, store, schema).with_cancellation(token.clone());
    let running = tokio::spawn(HarnessRun::start(
        RunConfig::new().with_cancel_grace_period(Duration::from_millis(200)),
        [pid("slow")],
        deps,
    ));
    tokio::time::sleep(Duration::from_millis(100)).await;

    token.cancel();
    let report = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("a cancelled run must end promptly")
        .unwrap()
        .unwrap();

    assert_eq!(report.status, RunStatus::Cancelled);
}

struct Spent(u64);

impl Meter for Spent {
    fn usage(&self) -> BTreeMap<String, u64> {
        BTreeMap::from([("tokens".to_string(), self.0)])
    }
}

#[tokio::test]
async fn a_usage_limit_ends_the_run_as_budget_exceeded() {
    let store: Arc<dyn RunStore> = Arc::new(dagrs::MemoryRunStore::new());
    let (schema, keys) = schema_with(&[("n", ReducerKind::Append)]);
    let registry = ParticipantRegistry::new();
    registry
        .register(Node::new("tick", &[], &["n"], &keys, |_, k| {
            Ok((write(k, "n", json!([1])), then(&["tick"])))
        }))
        .unwrap();
    let deps = dependencies(registry, store, schema).with_meter(Arc::new(Spent(1_000)));

    let report = HarnessRun::start(
        RunConfig::new().with_usage_limit("tokens", 500),
        [pid("tick")],
        deps,
    )
    .await
    .unwrap();

    assert_eq!(report.status, RunStatus::BudgetExceeded);
}

#[tokio::test]
async fn every_commit_is_a_durable_ordered_event() {
    let store: Arc<dyn RunStore> = Arc::new(dagrs::MemoryRunStore::new());
    let (schema, keys) = schema_with(&[("n", ReducerKind::Append)]);
    let registry = ParticipantRegistry::new();
    registry
        .register(Node::new("one", &[], &["n"], &keys, |_, k| {
            Ok((write(k, "n", json!([1])), then(&["two"])))
        }))
        .unwrap();
    registry
        .register(Node::new("two", &[], &[], &keys, |_, _| {
            Ok((StateDelta::new(), finish()))
        }))
        .unwrap();
    let report = HarnessRun::start(
        RunConfig::new().with_thread_id("events"),
        [pid("one")],
        dependencies(registry, Arc::clone(&store), schema),
    )
    .await
    .unwrap();

    let events = dagrs::RunEventSource::events_after(
        store.as_ref(),
        &report.run_id,
        dagrs::EventCursor::START,
        100,
    )
    .await
    .unwrap();

    let sequences: Vec<u64> = events
        .events()
        .iter()
        .map(|e| e.cursor().sequence())
        .collect();
    assert!(sequences.len() >= 4, "{sequences:?}");
    assert!(sequences.windows(2).all(|pair| pair[0] < pair[1]));
}

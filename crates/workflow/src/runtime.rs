use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dagrs::{
    DagrsError, DagrsResult, ErrorCode, HarnessRun, ParticipantId, ParticipantRegistry,
    ReducerKind, RunConfig, RunDependencies, RunStore, State, StateDelta, StateKey, StateSchema,
    StateSnapshot, StepOutcome, StepParticipant, StepTransition, WorkResults,
};
use jensen_harness_core::budget::Caps;
use jensen_harness_core::store::{
    EventKind, LedgerError, NewScope, ScopeKind, Store, StoreError, workflow::WorkflowRun,
};
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_promote::Isolation;
use jsonschema::Validator;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::error::RunError;
use crate::expr;
use crate::host::Host;
use crate::nodes::{self, Values};
use crate::reducer;
use crate::spec::{NodeSpec, Reducer, WorkflowSpec};
use crate::state;
use crate::validate::{Workflow, load};

const LIVE: &str = "__live";
const LABELS: &str = "__labels";
const OUTCOME: &str = "__outcome";
const START: &str = "wf.start";
const END: &str = "wf.end";
const SCHEMA_VERSION: u32 = 1;
const SOFT_PERCENT: u8 = 80;
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_millis(300);

pub struct Shared {
    pub run_id: String,
    pub store: Store,
    pub host: Arc<dyn Host>,
    pub workflow: Workflow,
    validators: BTreeMap<String, Validator>,
    failure: Mutex<Option<RunError>>,
}

impl Shared {
    pub fn new(
        run_id: String,
        store: Store,
        host: Arc<dyn Host>,
        workflow: Workflow,
    ) -> Result<Arc<Self>, RunError> {
        Ok(Arc::new(Self {
            run_id,
            store,
            host,
            validators: compile_validators(&workflow)?,
            workflow,
            failure: Mutex::new(None),
        }))
    }

    pub fn validate(&self, node: &str, channel: &str, value: &Value) -> Result<(), RunError> {
        let validator = self.validators.get(channel).ok_or_else(|| {
            RunError::State(state::StateError::UnknownChannel(channel.to_owned()))
        })?;
        validator.validate(value).map_err(|error| {
            RunError::State(state::StateError::SchemaViolation {
                node: node.to_owned(),
                channel: channel.to_owned(),
                message: error.to_string(),
            })
        })
    }

    pub fn isolation(&self) -> Result<&Isolation, RunError> {
        self.host.isolation().ok_or(RunError::NeedsRepository)
    }

    pub fn root_for(&self, id: &str) -> Result<PathBuf, RunError> {
        match self.workflow.writer_scope(id) {
            Some(writer) => Ok(self.isolation()?.prepare_node(&self.run_id, writer)?.path),
            None => Ok(self.host.worktree().to_path_buf()),
        }
    }

    pub fn tools_for(&self, id: &str, access: Access) -> Result<ToolSet, RunError> {
        self.host.tools(access, &self.root_for(id)?)
    }

    fn stash(&self, error: RunError) {
        if let Ok(mut slot) = self.failure.lock()
            && slot.is_none()
        {
            *slot = Some(error);
        }
    }

    fn take_failure(&self) -> Option<RunError> {
        self.failure.lock().ok().and_then(|mut slot| slot.take())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
    BudgetExceeded,
    DeadlineExceeded,
}

#[derive(Debug)]
pub struct RunReport {
    pub run_id: String,
    pub status: RunStatus,
    pub channels: Values,
    pub outcomes: BTreeMap<String, String>,
    pub failure: Option<RunError>,
    pub super_steps: u64,
    pub integration_branch: Option<String>,
    pub head: Option<String>,
}

struct Ended {
    dagrs_run_id: String,
    status: dagrs::RunStatus,
    cause: Option<String>,
    super_steps: u64,
}

pub struct StartRequest {
    pub run_id: String,
    pub inputs: Values,
    pub caps: Caps,
}

struct Keys {
    channels: BTreeMap<String, StateKey<Value>>,
    live: StateKey<Value>,
    labels: StateKey<Value>,
    outcome: StateKey<Value>,
}

fn pid(name: &str) -> DagrsResult<ParticipantId> {
    ParticipantId::new(name)
}

fn failed(message: impl Into<String>) -> DagrsError {
    DagrsError::new(ErrorCode::DgRun0006NodeExecutionFailed, message.into())
}

fn reducer_kind(reducer: Reducer) -> (ReducerKind, Option<&'static str>) {
    match reducer {
        Reducer::Overwrite => (ReducerKind::Overwrite, None),
        Reducer::Append => (ReducerKind::Custom("append".to_owned()), Some("append")),
        Reducer::SetUnion => (
            ReducerKind::Custom("set_union".to_owned()),
            Some("set_union"),
        ),
        Reducer::MergeMap => (
            ReducerKind::Custom("merge_map".to_owned()),
            Some("merge_map"),
        ),
        Reducer::Max => (ReducerKind::Custom("max".to_owned()), Some("max")),
    }
}

const CUSTOM_REDUCERS: [(&str, Reducer); 4] = [
    ("append", Reducer::Append),
    ("set_union", Reducer::SetUnion),
    ("merge_map", Reducer::MergeMap),
    ("max", Reducer::Max),
];

fn build_schema(workflow: &Workflow) -> DagrsResult<(StateSchema, Keys)> {
    let mut schema = StateSchema::new(SCHEMA_VERSION)?;
    for (name, which) in CUSTOM_REDUCERS {
        schema.register_reducer(name, move |old: &Value, new: &Value| {
            reducer::apply(which, Some(old.clone()), new.clone()).map_err(|error| {
                DagrsError::new(ErrorCode::DgStt0001StateConflict, error.to_string())
            })
        })?;
    }
    let mut channels = BTreeMap::new();
    for (name, channel) in &workflow.spec().channels {
        let (kind, _) = reducer_kind(channel.reducer);
        channels.insert(
            name.clone(),
            schema.register_key::<Value>(name.as_str(), SCHEMA_VERSION, kind)?,
        );
    }
    let merge = || ReducerKind::Custom("merge_map".to_owned());
    let live = schema.register_key::<Value>(LIVE, SCHEMA_VERSION, merge())?;
    let labels = schema.register_key::<Value>(LABELS, SCHEMA_VERSION, merge())?;
    let outcome = schema.register_key::<Value>(OUTCOME, SCHEMA_VERSION, merge())?;
    Ok((
        schema,
        Keys {
            channels,
            live,
            labels,
            outcome,
        },
    ))
}

fn object_of(snapshot: &StateSnapshot, key: &StateKey<Value>) -> DagrsResult<Map<String, Value>> {
    Ok(match snapshot.get(key)? {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    })
}

async fn read_all(
    shared: &Shared,
    keys: &Keys,
    snapshot: &StateSnapshot,
) -> Result<Values, RunError> {
    let mut all = Values::new();
    for (name, key) in &keys.channels {
        let stored = snapshot
            .get(key)
            .map_err(|error| RunError::Engine(error.to_string()))?;
        if let Some(value) = stored {
            all.insert(name.clone(), state::resolve(&shared.store, value).await?);
        }
    }
    Ok(all)
}

struct Start {
    participant: ParticipantId,
    keys: Arc<Keys>,
    seed: Values,
    first: Vec<ParticipantId>,
}

#[async_trait]
impl StepParticipant for Start {
    fn id(&self) -> ParticipantId {
        self.participant.clone()
    }

    fn read_keys(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn write_keys(&self) -> BTreeSet<String> {
        self.seed.keys().cloned().collect()
    }

    async fn run(&self, _snapshot: StateSnapshot, _work: WorkResults) -> DagrsResult<StepOutcome> {
        let mut delta = StateDelta::new();
        for (name, value) in &self.seed {
            let key = self
                .keys
                .channels
                .get(name)
                .ok_or_else(|| failed(format!("seed names unknown channel `{name}`")))?;
            delta.set(key, value.clone())?;
        }
        Ok(StepOutcome::new(
            delta,
            Vec::new(),
            StepTransition::continue_with(self.first.clone()),
        ))
    }
}

struct End(ParticipantId);

#[async_trait]
impl StepParticipant for End {
    fn id(&self) -> ParticipantId {
        self.0.clone()
    }

    fn read_keys(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    fn write_keys(&self) -> BTreeSet<String> {
        BTreeSet::new()
    }

    async fn run(&self, _snapshot: StateSnapshot, _work: WorkResults) -> DagrsResult<StepOutcome> {
        Ok(StepOutcome::new(
            StateDelta::new(),
            Vec::new(),
            StepTransition::finish(),
        ))
    }
}

struct Incoming {
    from: String,
    guard: Option<String>,
}

struct NodeRunner {
    participant: ParticipantId,
    shared: Arc<Shared>,
    keys: Arc<Keys>,
    id: String,
    node: NodeSpec,
    incoming: Vec<Incoming>,
    next: Vec<ParticipantId>,
    writes: BTreeSet<String>,
}

impl NodeRunner {
    async fn is_live(&self, snapshot: &StateSnapshot, all: &Values) -> Result<bool, RunError> {
        if self.incoming.is_empty() {
            return Ok(true);
        }
        let live = object_of(snapshot, &self.keys.live)
            .map_err(|error| RunError::Engine(error.to_string()))?;
        let labels = object_of(snapshot, &self.keys.labels)
            .map_err(|error| RunError::Engine(error.to_string()))?;
        if let NodeSpec::Publish(publish) = &self.node {
            let outcomes = object_of(snapshot, &self.keys.outcome)
                .map_err(|error| RunError::Engine(error.to_string()))?;
            if outcomes.get(&publish.from) != Some(&Value::String("ok".to_owned())) {
                return Ok(false);
            }
        }
        for edge in &self.incoming {
            if live.get(&edge.from) != Some(&Value::Bool(true)) {
                continue;
            }
            let Some(guard) = &edge.guard else {
                return Ok(true);
            };
            let variables = BTreeMap::from([
                ("channels", Value::Object(all.clone().into_iter().collect())),
                (
                    "label",
                    labels.get(&edge.from).cloned().unwrap_or(Value::Null),
                ),
            ]);
            let passed =
                expr::truth(guard, &variables).map_err(|message| RunError::Expression {
                    node: self.id.clone(),
                    message,
                })?;
            if passed {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn step(&self, snapshot: &StateSnapshot) -> Result<StepOutcome, RunError> {
        let all = read_all(&self.shared, &self.keys, snapshot).await?;
        let mut delta = StateDelta::new();
        let set = |delta: &mut StateDelta, key: &StateKey<Value>, value: Value| {
            delta
                .set(key, value)
                .map_err(|error| RunError::Engine(error.to_string()))
        };
        let transition = StepTransition::continue_with(self.next.clone());
        if !self.is_live(snapshot, &all).await? {
            nodes::journal_event(
                &self.shared,
                EventKind::NodeSkipped,
                json!({"node": self.id}),
            )
            .await?;
            set(
                &mut delta,
                &self.keys.live,
                json!({ self.id.clone(): false }),
            )?;
            return Ok(StepOutcome::new(delta, Vec::new(), transition));
        }
        let inputs: Values = match &self.node {
            NodeSpec::Loop(_) => all.clone(),
            other => other
                .reads()
                .iter()
                .filter_map(|name| all.get(name).map(|value| (name.clone(), value.clone())))
                .collect(),
        };
        let finished = nodes::execute(&self.shared, &self.id, &self.node, 0, &inputs).await?;
        for (channel, value) in finished.outputs {
            self.shared.validate(&self.id, &channel, &value)?;
            let overwrite = self
                .shared
                .workflow
                .spec()
                .channels
                .get(&channel)
                .is_some_and(|c| c.reducer == Reducer::Overwrite);
            let stored = if overwrite {
                state::externalize(&self.shared.store, value).await?
            } else {
                value
            };
            let key = self.keys.channels.get(&channel).ok_or_else(|| {
                RunError::State(state::StateError::UnknownChannel(channel.clone()))
            })?;
            set(&mut delta, key, stored)?;
        }
        set(
            &mut delta,
            &self.keys.live,
            json!({ self.id.clone(): true }),
        )?;
        set(
            &mut delta,
            &self.keys.outcome,
            json!({ self.id.clone(): finished.outcome }),
        )?;
        if let Some(label) = finished.label {
            set(
                &mut delta,
                &self.keys.labels,
                json!({ self.id.clone(): label }),
            )?;
        }
        Ok(StepOutcome::new(delta, Vec::new(), transition))
    }
}

#[async_trait]
impl StepParticipant for NodeRunner {
    fn id(&self) -> ParticipantId {
        self.participant.clone()
    }

    fn read_keys(&self) -> BTreeSet<String> {
        let mut keys: BTreeSet<String> = self.keys.channels.keys().cloned().collect();
        keys.extend([LIVE, LABELS, OUTCOME].map(str::to_owned));
        keys
    }

    fn write_keys(&self) -> BTreeSet<String> {
        self.writes.clone()
    }

    async fn run(&self, snapshot: StateSnapshot, _work: WorkResults) -> DagrsResult<StepOutcome> {
        match self.step(&snapshot).await {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                let message = error.to_string();
                self.shared.stash(error);
                Err(failed(message))
            }
        }
    }
}

fn effective_writes(workflow: &Workflow, id: &str) -> BTreeSet<String> {
    let spec = workflow.spec();
    let mut writes: BTreeSet<String> = match spec.nodes.get(id) {
        Some(NodeSpec::Loop(body)) => body
            .body
            .iter()
            .filter_map(|member| spec.nodes.get(member))
            .flat_map(NodeSpec::writes)
            .collect(),
        Some(node) => node.writes().into_iter().collect(),
        None => BTreeSet::new(),
    };
    writes.extend([LIVE, OUTCOME, LABELS].map(str::to_owned));
    writes
}

fn registry(
    shared: &Arc<Shared>,
    keys: &Arc<Keys>,
    seed: Values,
) -> DagrsResult<(ParticipantRegistry, ParticipantId)> {
    let workflow = &shared.workflow;
    let waves = workflow.waves();
    let registry = ParticipantRegistry::new();
    let first = waves
        .first()
        .map(|wave| {
            wave.iter()
                .map(|id| pid(id))
                .collect::<DagrsResult<Vec<_>>>()
        })
        .transpose()?
        .unwrap_or_default();
    registry.register(Start {
        participant: pid(START)?,
        keys: Arc::clone(keys),
        seed,
        first,
    })?;
    registry.register(End(pid(END)?))?;
    for (depth, wave) in waves.iter().enumerate() {
        let next = match waves.get(depth + 1) {
            Some(following) => following
                .iter()
                .map(|id| pid(id))
                .collect::<DagrsResult<Vec<_>>>()?,
            None => vec![pid(END)?],
        };
        for id in wave {
            let Some(node) = workflow.spec().nodes.get(id) else {
                return Err(failed(format!("wave names unknown node `{id}`")));
            };
            let incoming = workflow
                .spec()
                .edges
                .iter()
                .filter(|edge| &edge.to == id)
                .map(|edge| Incoming {
                    from: edge.from.clone(),
                    guard: edge.when.clone(),
                })
                .collect();
            registry.register(NodeRunner {
                participant: pid(id)?,
                shared: Arc::clone(shared),
                keys: Arc::clone(keys),
                id: id.clone(),
                node: node.clone(),
                incoming,
                next: next.clone(),
                writes: effective_writes(workflow, id),
            })?;
        }
    }
    Ok((registry, pid(START)?))
}

fn compile_validators(workflow: &Workflow) -> Result<BTreeMap<String, Validator>, RunError> {
    workflow
        .spec()
        .channels
        .iter()
        .map(|(name, channel)| {
            jsonschema::validator_for(&channel.schema)
                .map(|validator| (name.clone(), validator))
                .map_err(|error| {
                    RunError::State(state::StateError::SchemaViolation {
                        node: String::new(),
                        channel: name.clone(),
                        message: error.to_string(),
                    })
                })
        })
        .collect()
}

pub struct Runner {
    pub store: Store,
    pub host: Arc<dyn Host>,
    pub checkpoints: Arc<dyn RunStore>,
}

impl Runner {
    pub async fn start(
        &self,
        workflow: Workflow,
        request: StartRequest,
        cancel: CancellationToken,
    ) -> Result<RunReport, RunError> {
        let spec_json = serde_json::to_string(workflow.spec())
            .map_err(|error| RunError::Engine(error.to_string()))?;
        let shared = Shared::new(
            request.run_id.clone(),
            self.store.clone(),
            Arc::clone(&self.host),
            workflow,
        )?;
        let mut seed = Values::new();
        for (name, channel) in &shared.workflow.spec().channels {
            if let Some(init) = &channel.init {
                seed.insert(name.clone(), init.clone());
            }
        }
        for (name, value) in &request.inputs {
            shared
                .validate("inputs", name, value)
                .map_err(|error| RunError::BadInput {
                    channel: name.clone(),
                    message: error.to_string(),
                })?;
            seed.insert(name.clone(), value.clone());
        }
        match self
            .store
            .open_scope(NewScope {
                id: request.run_id.clone(),
                parent_id: None,
                kind: ScopeKind::Run,
                caps: request.caps,
                soft_percent: SOFT_PERCENT,
            })
            .await
        {
            Ok(()) | Err(StoreError::Ledger(LedgerError::ScopeExists(_))) => {}
            Err(error) => return Err(error.into()),
        }
        self.store
            .create_workflow_run(WorkflowRun {
                run_id: request.run_id.clone(),
                spec_hash: shared.workflow.hash().to_owned(),
                spec: spec_json,
                inputs: serde_json::to_string(&request.inputs)
                    .map_err(|error| RunError::Engine(error.to_string()))?,
                worktree: self.host.worktree().to_string_lossy().into_owned(),
            })
            .await?;
        if shared.workflow.needs_repository() {
            shared.isolation()?.begin_run(&shared.run_id)?;
        }
        nodes::journal_event(
            &shared,
            EventKind::WorkflowStarted,
            json!({"spec_hash": shared.workflow.hash()}),
        )
        .await?;
        self.drive(&shared, seed, cancel).await
    }

    fn prepare(
        shared: &Arc<Shared>,
        checkpoints: &Arc<dyn RunStore>,
        seed: Values,
        cancel: CancellationToken,
    ) -> Result<(RunConfig, ParticipantId, RunDependencies), RunError> {
        let (schema, keys) = build_schema(&shared.workflow)?;
        let keys = Arc::new(keys);
        let (participants, start) = registry(shared, &keys, seed)?;
        let initial: State = schema.empty_state();
        let dependencies =
            RunDependencies::new(participants, Arc::clone(checkpoints), schema, initial)?
                .with_cancellation(cancel);
        let steps = u64::try_from(shared.workflow.waves().len()).unwrap_or(u64::MAX);
        let mut config = RunConfig::new()
            .with_thread_id(shared.run_id.as_str())
            .with_max_super_steps(steps.saturating_add(3))
            .with_cancel_grace_period(CANCEL_GRACE);
        if let Some(limit) = shared.workflow.spec().limits.max_super_steps {
            config = config.with_max_super_steps(limit);
        }
        Ok((config, start, dependencies))
    }

    async fn drive(
        &self,
        shared: &Arc<Shared>,
        seed: Values,
        cancel: CancellationToken,
    ) -> Result<RunReport, RunError> {
        let (config, start, dependencies) = Self::prepare(shared, &self.checkpoints, seed, cancel)?;
        let report = HarnessRun::start(config, [start], dependencies).await?;
        self.report(
            shared,
            Ended {
                dagrs_run_id: report.run_id,
                status: report.status,
                cause: report.terminal_cause.map(|cause| cause.to_string()),
                super_steps: report.super_steps_completed,
            },
        )
        .await
    }

    pub async fn resume(
        &self,
        workflow: Workflow,
        run_id: &str,
        cancel: CancellationToken,
    ) -> Result<RunReport, RunError> {
        let record = self
            .store
            .workflow_run(run_id)
            .await?
            .ok_or_else(|| RunError::UnknownRun(run_id.to_owned()))?;
        if record.spec_hash != workflow.hash() {
            return Err(RunError::SpecChanged {
                run_id: run_id.to_owned(),
                recorded: record.spec_hash,
                offered: workflow.hash().to_owned(),
            });
        }
        let inputs: Values = serde_json::from_str(&record.inputs)
            .map_err(|error| RunError::Engine(error.to_string()))?;
        let shared = Shared::new(
            run_id.to_owned(),
            self.store.clone(),
            Arc::clone(&self.host),
            workflow,
        )?;
        let runs = self.checkpoints.list_runs(run_id, None, 1).await?;
        let latest = runs
            .runs()
            .first()
            .ok_or_else(|| RunError::UnknownRun(run_id.to_owned()))?;
        let checkpoint = self
            .checkpoints
            .load_latest_checkpoint_for_run(latest.run_id())
            .await?;
        if checkpoint.status() != dagrs::RunStatus::Running
            && checkpoint.status() != dagrs::RunStatus::Interrupted
        {
            return self
                .report(
                    &shared,
                    Ended {
                        dagrs_run_id: latest.run_id().to_owned(),
                        status: checkpoint.status(),
                        cause: checkpoint.terminal_error_message().map(str::to_owned),
                        super_steps: checkpoint.completed_super_steps(),
                    },
                )
                .await;
        }
        let mut seed = Values::new();
        for (name, channel) in &shared.workflow.spec().channels {
            if let Some(init) = &channel.init {
                seed.insert(name.clone(), init.clone());
            }
        }
        seed.extend(inputs);
        nodes::journal_event(
            &shared,
            EventKind::WorkflowStarted,
            json!({"resumed": true}),
        )
        .await?;
        let (_, _, dependencies) = Self::prepare(&shared, &self.checkpoints, seed, cancel)?;
        let report =
            HarnessRun::resume(run_id, latest.latest_checkpoint_id(), dependencies).await?;
        self.report(
            &shared,
            Ended {
                dagrs_run_id: report.run_id,
                status: report.status,
                cause: report.terminal_cause.map(|cause| cause.to_string()),
                super_steps: report.super_steps_completed,
            },
        )
        .await
    }

    pub async fn resume_recorded(
        &self,
        run_id: &str,
        cancel: CancellationToken,
    ) -> Result<RunReport, RunError> {
        let record = self
            .store
            .workflow_run(run_id)
            .await?
            .ok_or_else(|| RunError::UnknownRun(run_id.to_owned()))?;
        let spec: WorkflowSpec = serde_json::from_str(&record.spec)
            .map_err(|error| RunError::Engine(error.to_string()))?;
        self.resume(Workflow::new(spec)?, run_id, cancel).await
    }

    pub async fn status(&self, run_id: &str) -> Result<(String, RunStatus), RunError> {
        let record = self
            .store
            .workflow_run(run_id)
            .await?
            .ok_or_else(|| RunError::UnknownRun(run_id.to_owned()))?;
        let runs = self.checkpoints.list_runs(run_id, None, 1).await?;
        let Some(latest) = runs.runs().first() else {
            return Ok((record.spec_hash, RunStatus::Running));
        };
        let checkpoint = self
            .checkpoints
            .load_latest_checkpoint_for_run(latest.run_id())
            .await?;
        Ok((record.spec_hash, map_status(checkpoint.status())))
    }

    async fn release_in_flight(&self, shared: &Shared) -> Result<usize, RunError> {
        let open = self.store.open_reservations_under(&shared.run_id).await?;
        for call_id in &open {
            self.store.release(call_id, None).await?;
        }
        Ok(open.len())
    }

    async fn report(&self, shared: &Arc<Shared>, ended: Ended) -> Result<RunReport, RunError> {
        let failure = shared.take_failure();
        let status = match (&failure, ended.status) {
            (Some(RunError::BudgetExhausted { .. }), _) => RunStatus::BudgetExceeded,
            (_, other) => map_status(other),
        };
        let released = if status == RunStatus::Succeeded {
            0
        } else {
            self.release_in_flight(shared).await?
        };
        let (channels, outcomes) = self.final_state(shared, &ended.dagrs_run_id).await?;
        let failure = failure.or_else(|| {
            if status == RunStatus::Succeeded || status == RunStatus::Cancelled {
                None
            } else {
                ended.cause.map(RunError::Engine)
            }
        });
        nodes::journal_event(
            shared,
            EventKind::WorkflowFinished,
            json!({
                "status": format!("{status:?}"),
                "released_reservations": released,
                "cause": failure.as_ref().map(ToString::to_string),
            }),
        )
        .await?;
        let (integration_branch, head) = if shared.workflow.needs_repository() {
            let head = shared.isolation()?.integration_head(&shared.run_id)?;
            (Some(Isolation::integration_ref(&shared.run_id)), Some(head))
        } else {
            (None, None)
        };
        Ok(RunReport {
            run_id: shared.run_id.clone(),
            status,
            channels,
            outcomes,
            failure,
            super_steps: ended.super_steps,
            integration_branch,
            head,
        })
    }

    async fn final_state(
        &self,
        shared: &Arc<Shared>,
        dagrs_run_id: &str,
    ) -> Result<(Values, BTreeMap<String, String>), RunError> {
        let checkpoint = self
            .checkpoints
            .load_latest_checkpoint_for_run(dagrs_run_id)
            .await?;
        let mut schema = StateSchema::from_definition(checkpoint.state_schema().clone())?;
        let mut channels = Values::new();
        for name in shared.workflow.spec().channels.keys() {
            let key = schema.bind_key::<Value>(name.as_str(), SCHEMA_VERSION)?;
            if let Some(value) = checkpoint.state().get(&schema, &key)? {
                channels.insert(name.clone(), state::resolve(&shared.store, value).await?);
            }
        }
        let key = schema.bind_key::<Value>(OUTCOME, SCHEMA_VERSION)?;
        let outcomes = match checkpoint.state().get(&schema, &key)? {
            Some(Value::Object(map)) => map
                .into_iter()
                .filter_map(|(node, outcome)| outcome.as_str().map(|o| (node, o.to_owned())))
                .collect(),
            _ => BTreeMap::new(),
        };
        Ok((channels, outcomes))
    }
}

fn map_status(status: dagrs::RunStatus) -> RunStatus {
    match status {
        dagrs::RunStatus::Running | dagrs::RunStatus::Interrupted => RunStatus::Running,
        dagrs::RunStatus::Succeeded => RunStatus::Succeeded,
        dagrs::RunStatus::Cancelled => RunStatus::Cancelled,
        dagrs::RunStatus::BudgetExceeded => RunStatus::BudgetExceeded,
        dagrs::RunStatus::DeadlineExceeded => RunStatus::DeadlineExceeded,
        _ => RunStatus::Failed,
    }
}

pub fn parse_workflow(yaml: &str) -> Result<Workflow, RunError> {
    Ok(load(yaml)?)
}

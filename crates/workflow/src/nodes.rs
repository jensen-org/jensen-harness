use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use jensen_harness_core::agent::AgentError;
use jensen_harness_core::agent::{
    DEFAULT_MAX_TURNS, PromptStop, Recovered, Session, SessionConfig,
};
use jensen_harness_core::budget::Caps;
use jensen_harness_core::providers::{Provider, Role};
use jensen_harness_core::store::{
    EventKind, LedgerError, NewEvent, NewScope, ScopeKind, Store, StoreError,
};
use jensen_harness_core::toolset::Access;
use jensen_harness_promote::forge::open_request;
use jensen_harness_promote::push::push;
use jensen_harness_promote::{Finding, GateConfig, Gates, Isolation, PromoteError, Provenance};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::error::RunError;
use crate::expr;
use crate::reducer;
use crate::runtime::Shared;
use crate::spec::PublishNode;
use crate::spec::{
    AgentNode, CommandNode, LoopNode, NodeSpec, PromoteNode, RouterNode, ToolAccess, VerifierNode,
};

pub type Values = BTreeMap<String, Value>;

const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 120;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Finished {
    pub outputs: Values,
    pub label: Option<String>,
    pub outcome: String,
    #[serde(default)]
    pub detail: Option<Value>,
}

impl Finished {
    fn ok(outputs: Values) -> Self {
        Self {
            outputs,
            label: None,
            outcome: "ok".to_owned(),
            detail: None,
        }
    }
}

pub fn journal_session(run_id: &str) -> String {
    format!("wf:{run_id}")
}

fn is_idempotent(node: &NodeSpec) -> bool {
    match node {
        NodeSpec::Command(command) => command.idempotent,
        _ => true,
    }
}

fn channels_variable(inputs: &Values) -> Value {
    Value::Object(inputs.clone().into_iter().collect())
}

fn memo_key(
    shared: &Shared,
    id: &str,
    ordinal: u32,
    node: &NodeSpec,
    inputs: &Values,
) -> Result<String, RunError> {
    let definition =
        serde_json::to_value(node).map_err(|error| RunError::Engine(error.to_string()))?;
    let canonical = json!({
        "run": shared.run_id,
        "node": id,
        "ordinal": ordinal,
        "definition": definition,
        "inputs": inputs,
    })
    .to_string();
    Ok(blake3::hash(canonical.as_bytes()).to_hex().to_string())
}

async fn journal(
    store: &Store,
    run_id: &str,
    kind: EventKind,
    payload: Value,
) -> Result<(), RunError> {
    store
        .append(NewEvent {
            session_id: journal_session(run_id),
            parent_id: None,
            kind,
            payload,
        })
        .await?;
    Ok(())
}

pub async fn journal_event(
    shared: &Shared,
    kind: EventKind,
    payload: Value,
) -> Result<(), RunError> {
    journal(&shared.store, &shared.run_id, kind, payload).await
}

async fn started_without_finish(shared: &Shared, id: &str, ordinal: u32) -> Result<bool, RunError> {
    let events = shared
        .store
        .events(&journal_session(&shared.run_id))
        .await?;
    let mut open = false;
    for event in events {
        let same = event.payload.get("node").and_then(Value::as_str) == Some(id)
            && event.payload.get("ordinal").and_then(Value::as_u64) == Some(u64::from(ordinal));
        if !same {
            continue;
        }
        match event.kind {
            EventKind::NodeStarted => open = true,
            EventKind::NodeFinished => open = false,
            _ => {}
        }
    }
    Ok(open)
}

fn interrupted(node: &NodeSpec) -> Finished {
    let mut outputs = Values::new();
    if let NodeSpec::Command(command) = node
        && let Some(channel) = &command.output
    {
        outputs.insert(
            channel.clone(),
            json!({
                "exit_code": -1,
                "stdout": "",
                "stderr": "interrupted by a restart; the command may have partly run and was not run again",
            }),
        );
    }
    Finished {
        outputs,
        label: None,
        outcome: "interrupted".to_owned(),
        detail: None,
    }
}

pub async fn execute(
    shared: &Shared,
    id: &str,
    node: &NodeSpec,
    ordinal: u32,
    inputs: &Values,
) -> Result<Finished, RunError> {
    let key = memo_key(shared, id, ordinal, node, inputs)?;
    if let Some(stored) = shared.store.memoized_result(&key).await? {
        return serde_json::from_str(&stored).map_err(|error| RunError::Engine(error.to_string()));
    }
    let payload = json!({"node": id, "ordinal": ordinal, "kind": node.kind()});
    let finished = if !is_idempotent(node) && started_without_finish(shared, id, ordinal).await? {
        interrupted(node)
    } else {
        journal_event(shared, EventKind::NodeStarted, payload.clone()).await?;
        run_kind(shared, id, node, ordinal, inputs).await?
    };
    let encoded =
        serde_json::to_string(&finished).map_err(|error| RunError::Engine(error.to_string()))?;
    shared
        .store
        .record_node_result(&shared.run_id, id, &key, &encoded)
        .await?;
    journal_event(
        shared,
        EventKind::NodeFinished,
        json!({"node": id, "ordinal": ordinal, "outcome": finished.outcome, "detail": finished.detail}),
    )
    .await?;
    Ok(finished)
}

async fn run_kind(
    shared: &Shared,
    id: &str,
    node: &NodeSpec,
    ordinal: u32,
    inputs: &Values,
) -> Result<Finished, RunError> {
    match node {
        NodeSpec::Agent(agent) => run_agent(shared, id, ordinal, agent, inputs).await,
        NodeSpec::Command(command) => run_command(shared, id, command).await,
        NodeSpec::Verifier(verifier) => run_verifier(shared, id, verifier, inputs).await,
        NodeSpec::Router(router) => run_router(id, router, inputs),
        NodeSpec::Loop(looped) => Box::pin(run_loop(shared, id, looped, inputs)).await,
        NodeSpec::Promote(promote) => run_promote(shared, promote, inputs).await,
        NodeSpec::Publish(publish) => run_publish(shared, id, publish).await,
    }
}

fn bad_output(node: &str, message: impl Into<String>) -> RunError {
    RunError::BadOutput {
        node: node.to_owned(),
        message: message.into(),
    }
}

fn parse_object(node: &str, text: &str) -> Result<Map<String, Value>, RunError> {
    let trimmed = text.trim();
    let candidates = [
        Some(trimmed),
        trimmed
            .strip_prefix("```json")
            .or_else(|| trimmed.strip_prefix("```"))
            .and_then(|rest| rest.strip_suffix("```"))
            .map(str::trim),
        trimmed
            .find('{')
            .zip(trimmed.rfind('}'))
            .and_then(|(start, end)| trimmed.get(start..=end)),
    ];
    for candidate in candidates.into_iter().flatten() {
        if let Ok(Value::Object(map)) = serde_json::from_str(candidate) {
            return Ok(map);
        }
    }
    Err(bad_output(node, "no JSON object in the output"))
}

fn take_writes(
    node: &str,
    writes: &[String],
    mut object: Map<String, Value>,
) -> Result<Values, RunError> {
    let mut outputs = Values::new();
    for channel in writes {
        let value = object
            .remove(channel)
            .ok_or_else(|| bad_output(node, format!("missing key `{channel}`")))?;
        outputs.insert(channel.clone(), value);
    }
    if let Some(extra) = object.keys().next() {
        return Err(bad_output(node, format!("unexpected key `{extra}`")));
    }
    Ok(outputs)
}

fn compose_prompt(shared: &Shared, node: &AgentNode, inputs: &Values) -> String {
    let mut prompt = node.prompt.clone();
    if !inputs.is_empty() {
        prompt.push_str("\n\nInputs (JSON):\n");
        prompt.push_str(&channels_variable(inputs).to_string());
    }
    if !node.writes.is_empty() {
        prompt.push_str("\n\nReply with one JSON object and nothing else. Its keys are exactly these channels, each value matching its schema:");
        for channel in &node.writes {
            let schema = shared
                .workflow
                .spec()
                .channels
                .get(channel)
                .map_or(Value::Null, |c| c.schema.clone());
            prompt.push_str(&format!("\n- {channel}: {schema}"));
        }
    }
    prompt
}

async fn run_agent(
    shared: &Shared,
    id: &str,
    ordinal: u32,
    node: &AgentNode,
    inputs: &Values,
) -> Result<Finished, RunError> {
    let agent_error = |source: AgentError| RunError::Agent {
        node: id.to_owned(),
        source,
    };
    let binding = shared.host.bind(&node.role)?;
    let requested = node.budget.as_ref().map(|budget| Caps {
        max_tokens: budget.max_tokens,
        max_cost_micro_usd: budget.max_cost_micro_usd,
    });
    let settings = shared.host.budget();
    let budget = settings.resolve(requested, &binding.model_address)?;
    let node_scope = format!("{}:{id}:{ordinal}", shared.run_id);
    match shared
        .store
        .open_scope(NewScope {
            id: node_scope.clone(),
            parent_id: Some(shared.run_id.clone()),
            kind: ScopeKind::Node,
            caps: requested.unwrap_or_default(),
            soft_percent: budget.soft_percent,
        })
        .await
    {
        Ok(()) | Err(StoreError::Ledger(LedgerError::ScopeExists(_))) => {}
        Err(error) => return Err(error.into()),
    }
    let session_id = format!("{}.{id}.{ordinal}", shared.run_id);
    let access = match node.tools {
        ToolAccess::ReadOnly => Access::ReadOnly,
        ToolAccess::Writer => Access::ReadWrite,
    };
    let tools = shared.tools_for(id, access)?;
    let provider: Arc<dyn Provider> = Arc::clone(&binding.provider);
    let stop = match Recovered::load(&shared.store, &session_id).await {
        Ok(recovered) => {
            let open = recovered.has_open_prompt();
            let mut session = Session::restore(shared.store.clone(), recovered, tools)
                .await
                .map_err(agent_error)?;
            if open {
                let outcome = session
                    .resume(provider.as_ref())
                    .await
                    .map_err(agent_error)?;
                (session, Some(outcome.stop))
            } else if session.turns().is_empty() {
                let prompt = compose_prompt(shared, node, inputs);
                let outcome = session
                    .prompt(provider.as_ref(), &prompt)
                    .await
                    .map_err(agent_error)?;
                (session, Some(outcome.stop))
            } else {
                (session, None)
            }
        }
        Err(AgentError::UnknownSession(_)) => {
            let mut session = Session::create(
                shared.store.clone(),
                SessionConfig {
                    session_id: session_id.clone(),
                    run_id: Some(node_scope),
                    model_address: binding.model_address.clone(),
                    max_turns: node.max_turns.unwrap_or(DEFAULT_MAX_TURNS),
                    budget,
                    price_version: settings.pricing.version().to_owned(),
                },
                tools,
            )
            .await
            .map_err(agent_error)?;
            let prompt = compose_prompt(shared, node, inputs);
            let outcome = session
                .prompt(provider.as_ref(), &prompt)
                .await
                .map_err(agent_error)?;
            (session, Some(outcome.stop))
        }
        Err(error) => return Err(agent_error(error)),
    };
    let (session, stop) = stop;
    match stop {
        None | Some(PromptStop::EndTurn | PromptStop::StopSequence) => {}
        Some(PromptStop::BudgetExhausted) => {
            return Err(RunError::BudgetExhausted {
                node: id.to_owned(),
            });
        }
        Some(other) => {
            return Err(RunError::AgentStopped {
                node: id.to_owned(),
                reason: format!("{other:?}"),
            });
        }
    }
    if node.writes.is_empty() {
        return Ok(Finished::ok(Values::new()));
    }
    let text = session
        .turns()
        .iter()
        .rev()
        .find(|turn| turn.role == Role::Assistant)
        .map(jensen_harness_core::providers::Turn::plain_text)
        .ok_or_else(|| bad_output(id, "the agent never answered"))?;
    let object = parse_object(id, &text)?;
    Ok(Finished::ok(take_writes(id, &node.writes, object)?))
}

fn command_timeout(seconds: Option<u64>) -> Duration {
    Duration::from_secs(seconds.unwrap_or(DEFAULT_COMMAND_TIMEOUT_SECS))
}

async fn run_command(shared: &Shared, id: &str, node: &CommandNode) -> Result<Finished, RunError> {
    let tools = shared.tools_for(id, Access::ReadWrite)?;
    let result = tools
        .run_command(&node.run, command_timeout(node.timeout_secs))
        .await?;
    let mut outputs = Values::new();
    if let Some(channel) = &node.output {
        outputs.insert(
            channel.clone(),
            json!({"exit_code": result.exit_code, "stdout": result.stdout, "stderr": result.stderr}),
        );
    }
    if result.exit_code != 0 {
        return Ok(Finished {
            outputs,
            label: None,
            outcome: "failed".to_owned(),
            detail: None,
        });
    }
    if !node.writes.is_empty() {
        let object = parse_object(id, &result.stdout)?;
        outputs.extend(take_writes(id, &node.writes, object)?);
    }
    Ok(Finished::ok(outputs))
}

async fn run_verifier(
    shared: &Shared,
    id: &str,
    node: &VerifierNode,
    inputs: &Values,
) -> Result<Finished, RunError> {
    let tools = shared.tools_for(id, Access::ReadWrite)?;
    let result = tools
        .run_command(&node.run, command_timeout(node.timeout_secs))
        .await?;
    let variables = BTreeMap::from([
        ("exit_code", json!(result.exit_code)),
        ("stdout", json!(result.stdout)),
        ("stderr", json!(result.stderr)),
        ("channels", channels_variable(inputs)),
    ]);
    let verdict =
        expr::truth(&node.pass_when, &variables).map_err(|message| RunError::Expression {
            node: id.to_owned(),
            message,
        })?;
    Ok(Finished::ok(Values::from([(
        node.passes.clone(),
        Value::Bool(verdict),
    )])))
}

fn run_router(id: &str, node: &RouterNode, inputs: &Values) -> Result<Finished, RunError> {
    let variables = BTreeMap::from([("channels", channels_variable(inputs))]);
    let label = expr::label(&node.expr, &variables).map_err(|message| RunError::Expression {
        node: id.to_owned(),
        message,
    })?;
    Ok(Finished {
        outputs: Values::new(),
        label: Some(label),
        outcome: "ok".to_owned(),
        detail: None,
    })
}

async fn run_loop(
    shared: &Shared,
    id: &str,
    node: &LoopNode,
    inputs: &Values,
) -> Result<Finished, RunError> {
    let spec = shared.workflow.spec();
    let max_iterations = node.max_iterations.unwrap_or(1);
    let mut view = inputs.clone();
    let mut emitted = Values::new();
    let mut previous: Option<String> = None;
    let mut outcome = "exhausted";
    for iteration in 0..max_iterations {
        let mut produced = BTreeMap::new();
        for member in &node.body {
            let member_spec = spec.nodes.get(member).ok_or_else(|| {
                RunError::Engine(format!("loop `{id}` body names unknown node `{member}`"))
            })?;
            let reads: BTreeSet<&String> = member_spec.reads().iter().collect();
            let member_inputs: Values = view
                .iter()
                .filter(|(name, _)| reads.contains(name))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            let finished = Box::pin(execute(
                shared,
                member,
                member_spec,
                iteration,
                &member_inputs,
            ))
            .await?;
            for (channel, value) in finished.outputs {
                shared.validate(member, &channel, &value)?;
                let reducer = spec
                    .channels
                    .get(&channel)
                    .map(|c| c.reducer)
                    .unwrap_or_default();
                let applied = reducer::apply(reducer, view.remove(&channel), value.clone())
                    .map_err(|error| bad_output(member, error.to_string()))?;
                view.insert(channel.clone(), applied);
                let merged = reducer::apply(reducer, emitted.remove(&channel), value.clone())
                    .map_err(|error| bad_output(member, error.to_string()))?;
                emitted.insert(channel.clone(), merged);
                produced.insert(channel, value);
            }
        }
        let fingerprint = serde_json::to_string(&produced)
            .map_err(|error| RunError::Engine(error.to_string()))?;
        let variables = BTreeMap::from([
            ("channels", channels_variable(&view)),
            ("iteration", json!(iteration + 1)),
        ]);
        let done =
            expr::truth(&node.until, &variables).map_err(|message| RunError::Expression {
                node: id.to_owned(),
                message,
            })?;
        if done {
            outcome = "ok";
            break;
        }
        if previous.as_deref() == Some(fingerprint.as_str()) {
            outcome = "stalled";
            break;
        }
        previous = Some(fingerprint);
    }
    Ok(Finished {
        outputs: emitted,
        label: None,
        outcome: outcome.to_owned(),
        detail: None,
    })
}

fn writer_model(shared: &Shared, writer: &str) -> Result<String, RunError> {
    let spec = shared.workflow.spec();
    let agent_role = |id: &str| match spec.nodes.get(id) {
        Some(NodeSpec::Agent(agent)) => Some(agent.role.clone()),
        _ => None,
    };
    let role = match spec.nodes.get(writer) {
        Some(NodeSpec::Loop(body)) => body.body.iter().find_map(|member| agent_role(member)),
        _ => agent_role(writer),
    };
    match role {
        Some(role) => Ok(shared.host.bind(&role)?.model_address),
        None => Ok("none".to_owned()),
    }
}

fn blocked(output: Option<&String>, files: usize, findings: Vec<Value>, outcome: &str) -> Finished {
    let value = json!({"commit": null, "files": files, "flags": [], "blocked": findings});
    Finished {
        outputs: output
            .map(|channel| Values::from([(channel.clone(), value)]))
            .unwrap_or_default(),
        label: None,
        outcome: outcome.to_owned(),
        detail: Some(json!({"findings": findings})),
    }
}

fn finding_json(finding: &Finding) -> Value {
    json!({"gate": finding.gate, "path": finding.path, "reason": finding.reason})
}

async fn run_promote(
    shared: &Shared,
    node: &PromoteNode,
    inputs: &Values,
) -> Result<Finished, RunError> {
    let isolation = shared.isolation()?;
    let output = node.output.as_ref();
    let tampered = |error: PromoteError| match error {
        PromoteError::TamperedWorktree { reason, .. } => Ok(blocked(
            output,
            0,
            vec![json!({"gate": "worktree", "path": "", "reason": reason})],
            "blocked",
        )),
        other => Err(RunError::from(other)),
    };
    let handle = match isolation.prepare_node(&shared.run_id, &node.from) {
        Ok(handle) => handle,
        Err(error) => return tampered(error),
    };
    let provenance = Provenance {
        run: shared.run_id.clone(),
        node: node.from.clone(),
        session: format!("{}.{}.0", shared.run_id, node.from),
        model: writer_model(shared, &node.from)?,
        spec: shared.workflow.hash().to_owned(),
    };
    let snapshot = match isolation.snapshot(&handle, &provenance) {
        Ok(snapshot) => snapshot,
        Err(error) => return tampered(error),
    };
    let mut config = GateConfig {
        protected: node.protected.clone(),
        allow_binary: node.allow_binary,
        ..GateConfig::default()
    };
    if let Some(limit) = node.max_changed_files {
        config.max_changed_files = limit;
    }
    if let Some(limit) = node.max_added_bytes {
        config.max_added_bytes = limit;
    }
    let unmet: Vec<String> = node
        .requires
        .iter()
        .filter(|channel| inputs.get(*channel) != Some(&Value::Bool(true)))
        .cloned()
        .collect();
    let gates = Gates::new(config)?;
    let review = gates.review(isolation, &shared.run_id, &snapshot.commit, &unmet)?;
    let flags: Vec<Value> = review.report.flags.iter().map(finding_json).collect();
    let Some(cleared) = review.cleared else {
        let findings = review.report.blocks.iter().map(finding_json).collect();
        return Ok(blocked(output, review.report.files, findings, "blocked"));
    };
    let files = review.report.files;
    let promoted = match isolation.merge_into_integration(
        &shared.run_id,
        &cleared,
        &gates,
        &provenance,
    ) {
        Ok(promoted) => promoted,
        Err(PromoteError::PromotionConflict { files: names, .. }) => {
            let finding = json!({"gate": "merge", "path": names, "reason": "the work conflicts with the integration branch"});
            return Ok(blocked(output, files, vec![finding], "conflict"));
        }
        Err(PromoteError::MergedResultBlocked { reasons, .. }) => {
            let finding = json!({"gate": "merged", "path": "", "reason": reasons});
            return Ok(blocked(output, files, vec![finding], "blocked"));
        }
        Err(error) => return Err(error.into()),
    };
    isolation.remove_node(&handle)?;
    let value = json!({
        "commit": promoted.head,
        "node_commit": snapshot.commit,
        "files": files,
        "flags": flags,
    });
    Ok(Finished {
        outputs: output
            .map(|channel| Values::from([(channel.clone(), value)]))
            .unwrap_or_default(),
        label: None,
        outcome: "ok".to_owned(),
        detail: Some(json!({"flags": flags, "how": format!("{:?}", promoted.how)})),
    })
}

async fn run_publish(shared: &Shared, id: &str, node: &PublishNode) -> Result<Finished, RunError> {
    let isolation = shared.isolation()?;
    let remote = isolation.remote_url("origin")?;
    let target = shared.host.forge(&remote)?;
    let head = format!("jensen/{}", shared.run_id);
    let base = isolation.base_branch()?;
    journal_event(
        shared,
        EventKind::PublishStarted,
        json!({"node": id, "branch": head, "forge": target.kind.as_str(), "project": target.project}),
    )
    .await?;
    push(
        isolation.repo_path(),
        &remote,
        &Isolation::integration_ref(&shared.run_id),
        &head,
        &target,
    )
    .await?;
    let published = open_request(&target, &head, &base, &node.title, &node.body).await?;
    journal_event(
        shared,
        EventKind::PublishFinished,
        json!({"node": id, "url": published.url, "number": published.number, "existing": published.existing}),
    )
    .await?;
    let value = json!({
        "forge": published.forge.as_str(),
        "url": published.url,
        "number": published.number,
        "existing": published.existing,
    });
    Ok(Finished {
        outputs: node
            .output
            .as_ref()
            .map(|channel| Values::from([(channel.clone(), value)]))
            .unwrap_or_default(),
        label: None,
        outcome: "ok".to_owned(),
        detail: None,
    })
}

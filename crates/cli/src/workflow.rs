use std::path::{Path, PathBuf};
use std::sync::Arc;

use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::budget::Caps;
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_extensions::{Extensions, SkillCatalog};
use jensen_harness_promote::Isolation;
use jensen_harness_promote::forge::ForgeTarget;
use jensen_harness_protocol::{
    RequestId, Response, WorkflowCancelParams, WorkflowCancelResult, WorkflowResumeParams,
    WorkflowRunResult, WorkflowStartParams, WorkflowStatus, WorkflowStatusParams,
    WorkflowStatusResult,
};
use jensen_harness_workflow::{
    CancellationToken, Host, RoleBinding, RunError, RunReport, RunStatus, Runner, StartRequest,
};
use serde_json::{Value, json};

use crate::engine::Engine;
use crate::rpc::{internal, invalid_params, parse_params};

struct EngineHost {
    engine: Arc<Engine>,
    worktree: PathBuf,
    isolation: Option<Isolation>,
    extensions: Extensions,
}

impl Host for EngineHost {
    fn bind(&self, role: &str) -> Result<RoleBinding, RunError> {
        let address = self
            .engine
            .models
            .role(role)
            .ok_or_else(|| RunError::UnknownRole(role.to_owned()))?;
        let resolved = self.engine.models.resolve(Some(&address))?;
        Ok(RoleBinding {
            provider: resolved.provider,
            model_address: resolved.address,
        })
    }

    fn tools(&self, access: Access, root: &Path) -> Result<ToolSet, RunError> {
        ToolSet::for_worktree_access(root, access)
            .map_err(|error| RunError::Engine(format!("worktree {}: {error}", root.display())))
    }

    fn isolation(&self) -> Option<&Isolation> {
        self.isolation.as_ref()
    }

    fn forge(&self, remote_url: &str) -> Result<ForgeTarget, RunError> {
        self.engine.models.forge_target(remote_url)
    }

    fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    fn worktree(&self) -> &Path {
        &self.worktree
    }

    fn budget(&self) -> &BudgetSettings {
        &self.engine.budget
    }
}

fn skill_roots(engine: &Engine, worktree: &Path) -> Vec<PathBuf> {
    let mut roots = vec![worktree.join("skills")];
    roots.extend(dirs::config_dir().map(|base| base.join("jensen-harness").join("skills")));
    roots.extend(engine.library.iter().map(|dir| dir.join("skills")));
    roots
}

fn runner(engine: &Arc<Engine>, worktree: PathBuf) -> Result<Runner, RunError> {
    let skills = SkillCatalog::discover(&skill_roots(engine, &worktree)).map_err(|source| {
        RunError::Skill {
            node: String::new(),
            source,
        }
    })?;
    Ok(Runner {
        store: engine.store.clone(),
        host: Arc::new(EngineHost {
            engine: Arc::clone(engine),
            isolation: Isolation::open(&worktree, &engine.workspaces).ok(),
            extensions: Extensions { skills },
            worktree,
        }),
        checkpoints: Arc::clone(&engine.checkpoints),
    })
}

fn status_of(status: RunStatus) -> WorkflowStatus {
    match status {
        RunStatus::Running => WorkflowStatus::Running,
        RunStatus::Succeeded => WorkflowStatus::Succeeded,
        RunStatus::Failed => WorkflowStatus::Failed,
        RunStatus::Cancelled => WorkflowStatus::Cancelled,
        RunStatus::BudgetExceeded => WorkflowStatus::BudgetExceeded,
        RunStatus::DeadlineExceeded => WorkflowStatus::DeadlineExceeded,
    }
}

fn run_error(id: RequestId, error: &RunError) -> Response {
    match error {
        RunError::Spec(_)
        | RunError::BadInput { .. }
        | RunError::UnknownRun(_)
        | RunError::SpecChanged { .. }
        | RunError::UnknownRole(_) => invalid_params(id, error.to_string()),
        _ => internal(id, error.to_string()),
    }
}

fn run_response(id: RequestId, report: RunReport) -> Response {
    Response::ok(
        id,
        json!(WorkflowRunResult {
            run_id: report.run_id,
            status: status_of(report.status),
            channels: report.channels.into_iter().collect(),
            outcomes: report.outcomes,
            failure: report.failure.map(|failure| failure.to_string()),
            super_steps: report.super_steps,
            integration_branch: report.integration_branch,
            head: report.head,
        }),
    )
}

async fn track(engine: &Arc<Engine>, run_id: &str) -> Result<CancellationToken, String> {
    let mut active = engine.active_workflows.lock().await;
    if active.contains_key(run_id) {
        return Err(format!("workflow run {run_id} is already running"));
    }
    let token = CancellationToken::new();
    active.insert(run_id.to_owned(), token.clone());
    Ok(token)
}

pub async fn workflow_start(
    engine: &Arc<Engine>,
    id: RequestId,
    params: Option<Value>,
) -> Response {
    let params: WorkflowStartParams = match parse_params("workflow.start", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    let worktree = PathBuf::from(&params.worktree);
    let file = worktree.join(&params.workflow);
    let yaml = match std::fs::read_to_string(&file) {
        Ok(yaml) => yaml,
        Err(error) => {
            return invalid_params(id, format!("workflow {}: {error}", file.display()));
        }
    };
    let library = jensen_harness_workflow::library::Library::for_workflow(
        &file,
        &engine.library,
        dirs::config_dir().map(|base| base.join("jensen-harness").join("agents")),
    );
    let workflow = match jensen_harness_workflow::load_with(&yaml, &library) {
        Ok(workflow) => workflow,
        Err(error) => return invalid_params(id, error.to_string()),
    };
    let caps = match params
        .budget
        .map(|budget| Caps {
            max_tokens: budget.max_tokens,
            max_cost_micro_usd: budget.max_cost_micro_usd,
        })
        .unwrap_or_default()
        .validated()
    {
        Ok(caps) => caps,
        Err(error) => return invalid_params(id, error.to_string()),
    };
    let run_id = uuid::Uuid::new_v4().to_string();
    let token = match track(engine, &run_id).await {
        Ok(token) => token,
        Err(message) => return invalid_params(id, message),
    };
    let result = match runner(engine, worktree) {
        Ok(runner) => {
            runner
                .start(
                    workflow,
                    StartRequest {
                        run_id: run_id.clone(),
                        inputs: params.inputs.into_iter().collect(),
                        caps,
                    },
                    token,
                )
                .await
        }
        Err(error) => Err(error),
    };
    engine.active_workflows.lock().await.remove(&run_id);
    match result {
        Ok(report) => run_response(id, report),
        Err(error) => run_error(id, &error),
    }
}

pub async fn workflow_status(
    engine: &Arc<Engine>,
    id: RequestId,
    params: Option<Value>,
) -> Response {
    let params: WorkflowStatusParams = match parse_params("workflow.status", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    let record = match engine.store.workflow_run(&params.run_id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return invalid_params(id, format!("workflow run {} does not exist", params.run_id));
        }
        Err(error) => return internal(id, error.to_string()),
    };
    let built = match runner(engine, PathBuf::from(record.worktree)) {
        Ok(runner) => runner,
        Err(error) => return run_error(id, &error),
    };
    match built.status(&params.run_id).await {
        Ok((spec_hash, status)) => Response::ok(
            id,
            json!(WorkflowStatusResult {
                run_id: params.run_id,
                spec_hash,
                status: status_of(status),
            }),
        ),
        Err(error) => run_error(id, &error),
    }
}

pub async fn workflow_resume(
    engine: &Arc<Engine>,
    id: RequestId,
    params: Option<Value>,
) -> Response {
    let params: WorkflowResumeParams = match parse_params("workflow.resume", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    let record = match engine.store.workflow_run(&params.run_id).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return invalid_params(id, format!("workflow run {} does not exist", params.run_id));
        }
        Err(error) => return internal(id, error.to_string()),
    };
    let token = match track(engine, &params.run_id).await {
        Ok(token) => token,
        Err(message) => return invalid_params(id, message),
    };
    let result = match runner(engine, PathBuf::from(record.worktree)) {
        Ok(runner) => runner.resume_recorded(&params.run_id, token).await,
        Err(error) => Err(error),
    };
    engine.active_workflows.lock().await.remove(&params.run_id);
    match result {
        Ok(report) => run_response(id, report),
        Err(error) => run_error(id, &error),
    }
}

pub async fn workflow_cancel(
    engine: &Arc<Engine>,
    id: RequestId,
    params: Option<Value>,
) -> Response {
    let params: WorkflowCancelParams = match parse_params("workflow.cancel", params) {
        Ok(params) => params,
        Err(message) => return invalid_params(id, message),
    };
    let active = engine.active_workflows.lock().await;
    let cancelled = active.get(&params.run_id).is_some_and(|token| {
        token.cancel();
        true
    });
    Response::ok(id, json!(WorkflowCancelResult { cancelled }))
}

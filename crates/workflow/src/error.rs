use jensen_harness_core::agent::AgentError;
use jensen_harness_core::budget::BudgetError;
use jensen_harness_core::providers::ProviderError;
use jensen_harness_core::store::StoreError;
use jensen_harness_core::toolset::ToolSetError;
use thiserror::Error;

use jensen_harness_promote::PromoteError;

use crate::state::StateError;
use crate::validate::SpecError;

#[derive(Debug, Error)]
pub enum RunError {
    #[error(transparent)]
    Spec(#[from] SpecError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    State(#[from] StateError),
    #[error("node `{node}` agent failed: {source}")]
    Agent { node: String, source: AgentError },
    #[error(transparent)]
    Tool(#[from] ToolSetError),
    #[error(transparent)]
    Budget(#[from] BudgetError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Promote(#[from] PromoteError),
    #[error("this workflow has writer or promote nodes and needs a git repository to isolate them")]
    NeedsRepository,
    #[error("no model is bound to role `{0}`")]
    UnknownRole(String),
    #[error("node `{node}` ran out of its token or cost budget")]
    BudgetExhausted { node: String },
    #[error("node `{node}` stopped before finishing: {reason}")]
    AgentStopped { node: String, reason: String },
    #[error("node `{node}` produced output the workflow cannot use: {message}")]
    BadOutput { node: String, message: String },
    #[error("expression of node `{node}` failed: {message}")]
    Expression { node: String, message: String },
    #[error("input for channel `{channel}` is refused: {message}")]
    BadInput { channel: String, message: String },
    #[error("the workflow engine failed: {0}")]
    Engine(String),
    #[error("workflow run {0} does not exist")]
    UnknownRun(String),
    #[error("workflow run {run_id} was started from spec {recorded}, not {offered}")]
    SpecChanged {
        run_id: String,
        recorded: String,
        offered: String,
    },
}

impl From<dagrs::DagrsError> for RunError {
    fn from(error: dagrs::DagrsError) -> Self {
        Self::Engine(error.to_string())
    }
}

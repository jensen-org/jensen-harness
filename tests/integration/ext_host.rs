use std::path::PathBuf;
use std::sync::Arc;

use dagrs::MemoryRunStore;
use jensen_harness_core::budget::{BudgetSettings, Caps};
use jensen_harness_core::providers::Provider;
use jensen_harness_core::store::Store;
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_extensions::Extensions;
use jensen_harness_promote::Isolation;
use jensen_harness_promote::forge::ForgeTarget;
use jensen_harness_workflow::nodes::Values;
use jensen_harness_workflow::{
    Host, RoleBinding, RunError, RunReport, Runner, StartRequest, Workflow,
};
use tokio_util::sync::CancellationToken;

use crate::common::tool_set;

pub(crate) struct ExtHost {
    pub provider: Arc<dyn Provider>,
    pub dir: PathBuf,
    pub extensions: Extensions,
    pub budget: BudgetSettings,
}

impl Host for ExtHost {
    fn bind(&self, _role: &str) -> Result<RoleBinding, RunError> {
        Ok(RoleBinding {
            provider: Arc::clone(&self.provider),
            model_address: "scripted/m".to_owned(),
        })
    }

    fn tools(&self, access: Access, root: &std::path::Path) -> Result<ToolSet, RunError> {
        let tools = tool_set(root);
        Ok(match access {
            Access::ReadOnly => tools.read_only(),
            Access::ReadWrite => tools,
        })
    }

    fn worktree(&self) -> &std::path::Path {
        &self.dir
    }

    fn isolation(&self) -> Option<&Isolation> {
        None
    }

    fn forge(&self, remote_url: &str) -> Result<ForgeTarget, RunError> {
        Err(jensen_harness_promote::PromoteError::UnknownForge(remote_url.to_owned()).into())
    }

    fn extensions(&self) -> &Extensions {
        &self.extensions
    }

    fn budget(&self) -> &BudgetSettings {
        &self.budget
    }
}

pub(crate) async fn run(
    host: ExtHost,
    workflow: Workflow,
    run_id: &str,
) -> Result<RunReport, RunError> {
    let runner = Runner {
        store: Store::in_memory().unwrap(),
        host: Arc::new(host),
        checkpoints: Arc::new(MemoryRunStore::new()),
    };
    runner
        .start(
            workflow,
            StartRequest {
                run_id: run_id.to_owned(),
                inputs: Values::new(),
                caps: Caps::default(),
            },
            CancellationToken::new(),
        )
        .await
}

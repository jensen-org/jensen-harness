use std::path::Path;
use std::sync::Arc;

use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::Provider;
use jensen_harness_core::toolset::{Access, ToolSet};
use jensen_harness_promote::Isolation;
use jensen_harness_promote::forge::ForgeTarget;

use crate::error::RunError;

pub struct RoleBinding {
    pub provider: Arc<dyn Provider>,
    pub model_address: String,
}

pub trait Host: Send + Sync {
    fn bind(&self, role: &str) -> Result<RoleBinding, RunError>;

    fn tools(&self, access: Access, root: &Path) -> Result<ToolSet, RunError>;

    fn worktree(&self) -> &Path;

    fn isolation(&self) -> Option<&Isolation>;

    fn forge(&self, remote_url: &str) -> Result<ForgeTarget, RunError>;

    fn budget(&self) -> &BudgetSettings;
}

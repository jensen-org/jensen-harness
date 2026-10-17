use std::path::Path;
use std::sync::Arc;

use jensen_harness_core::budget::BudgetSettings;
use jensen_harness_core::providers::Provider;
use jensen_harness_core::toolset::ToolSet;

use crate::error::RunError;

pub struct RoleBinding {
    pub provider: Arc<dyn Provider>,
    pub model_address: String,
}

pub trait Host: Send + Sync {
    fn bind(&self, role: &str) -> Result<RoleBinding, RunError>;

    fn tools(&self) -> Result<ToolSet, RunError>;

    fn worktree(&self) -> &Path;

    fn budget(&self) -> &BudgetSettings;
}

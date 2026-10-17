pub mod lsp;
pub mod mcp;
pub mod skills;

use std::sync::Arc;

pub use lsp::{
    ConfiguredLsp, LspDirectory, LspError, LspKind, LspRequest, LspServerSpec, LspSpecs,
};
pub use mcp::{McpDirectory, McpError, McpProvider, McpServerSpec, McpTransportSpec, Secret};
pub use skills::{Skill, SkillCatalog, SkillError, SkillProvider};

#[derive(Clone, Default)]
pub struct Extensions {
    pub skills: SkillCatalog,
    pub mcp: Option<Arc<dyn McpDirectory>>,
    pub lsp: Option<Arc<dyn LspDirectory>>,
}

impl std::fmt::Debug for Extensions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Extensions")
            .field("skills", &self.skills)
            .field("mcp", &self.mcp.is_some())
            .field("lsp", &self.lsp.is_some())
            .finish()
    }
}

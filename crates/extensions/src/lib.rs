pub mod skills;

pub use skills::{Skill, SkillCatalog, SkillError, SkillProvider};

#[derive(Debug, Clone, Default)]
pub struct Extensions {
    pub skills: SkillCatalog,
}

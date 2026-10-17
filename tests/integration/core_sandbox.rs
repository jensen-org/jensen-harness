use jensen_harness_core::sandbox::{FsMode, SandboxPolicy};
use std::path::Path;

#[test]
fn confined_to_denies_the_worktrees_own_git_pointer() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let git_rule = policy.extra_rules.first().unwrap();
    assert_eq!(git_rule.path, Path::new("/repo/task/.git"));
    assert_eq!(git_rule.mode, FsMode::Deny);
}

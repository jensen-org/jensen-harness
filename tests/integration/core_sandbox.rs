use jensen_harness_core::sandbox::{FsMode, SandboxPolicy};
use std::path::Path;

#[test]
fn confined_to_denies_the_worktrees_own_git_pointer() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let git_rule = policy.extra_rules.first().unwrap();
    assert_eq!(git_rule.path, Path::new("/repo/task/.git"));
    assert_eq!(git_rule.mode, FsMode::Deny);
}

#[test]
fn read_only_makes_the_worktree_unwritable_but_still_denies_its_git_pointer() {
    let policy = SandboxPolicy::read_only("/repo/task");
    let modes: Vec<(&Path, FsMode)> = policy
        .extra_rules
        .iter()
        .map(|rule| (rule.path.as_path(), rule.mode))
        .collect();
    assert_eq!(
        modes,
        [
            (Path::new("/repo/task"), FsMode::ReadOnly),
            (Path::new("/repo/task/.git"), FsMode::Deny),
        ]
    );
}

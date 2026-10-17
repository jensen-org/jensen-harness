use jensen_harness_core::sandbox::{FsMode, SandboxPolicy, is_within};
use std::path::Path;

#[test]
fn confined_to_denies_the_worktrees_own_git_pointer() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let git_rule = policy.extra_rules.first().unwrap();
    assert_eq!(git_rule.path, Path::new("/repo/task/.git"));
    assert_eq!(git_rule.mode, FsMode::Deny);
}

#[test]
fn is_within_accepts_a_nested_path() {
    assert!(is_within(
        Path::new("/repo/task"),
        Path::new("/repo/task/a.txt")
    ));
}

#[test]
fn is_within_rejects_a_sibling_path() {
    assert!(!is_within(
        Path::new("/repo/task"),
        Path::new("/repo/other/a.txt")
    ));
}

#[test]
fn is_within_rejects_parent_traversal_back_out() {
    assert!(!is_within(
        Path::new("/repo/task"),
        Path::new("/repo/task/../other")
    ));
}

use jensen_harness_core::tools::{ToolError, edit};
use std::fs;
use tempfile::tempdir;

#[test]
fn replaces_the_first_match() {
    let worktree = tempdir().unwrap();
    fs::write(worktree.path().join("a.txt"), "hello world").unwrap();
    edit::run(worktree.path(), "a.txt", "world", "there").unwrap();
    assert_eq!(
        fs::read_to_string(worktree.path().join("a.txt")).unwrap(),
        "hello there"
    );
}

#[test]
fn refuses_to_touch_the_worktrees_own_git_pointer() {
    let worktree = tempdir().unwrap();
    fs::write(worktree.path().join(".git"), "gitdir: /real").unwrap();
    assert!(edit::run(worktree.path(), ".git", "real", "fake").is_err());
}

#[test]
fn refuses_a_git_pointer_edit_disguised_by_a_traversal_prefix() {
    let worktree = tempdir().unwrap();
    fs::write(worktree.path().join(".git"), "gitdir: /real").unwrap();
    fs::create_dir(worktree.path().join("src")).unwrap();
    // The raw string does not start with ".git", but resolves there.
    assert!(edit::run(worktree.path(), "src/../.git", "real", "fake").is_err());
}

#[test]
fn refuses_a_path_outside_the_worktree() {
    let worktree = tempdir().unwrap();
    assert!(edit::run(worktree.path(), "../a.txt", "x", "y").is_err());
}

#[cfg(unix)]
#[test]
fn refuses_a_hard_link_alias_of_the_worktrees_own_git_pointer() {
    let worktree = tempdir().unwrap();
    let git_pointer = worktree.path().join(".git");
    fs::write(&git_pointer, "gitdir: /real").unwrap();
    let alias = worktree.path().join("not-git-at-all.txt");
    fs::hard_link(&git_pointer, &alias).unwrap();

    let error = edit::run(worktree.path(), "not-git-at-all.txt", "real", "fake").unwrap_err();

    assert!(matches!(error, ToolError::OutsideWorktree { .. }));
    assert_eq!(fs::read_to_string(&git_pointer).unwrap(), "gitdir: /real");
}

#[cfg(unix)]
#[test]
fn refuses_any_file_with_more_than_one_hard_link_regardless_of_what_it_aliases() {
    let worktree = tempdir().unwrap();
    let plain = worktree.path().join("plain.txt");
    fs::write(&plain, "hello").unwrap();
    let alias = worktree.path().join("alias.txt");
    fs::hard_link(&plain, &alias).unwrap();

    let error = edit::run(worktree.path(), "alias.txt", "hello", "bye").unwrap_err();

    assert!(matches!(error, ToolError::OutsideWorktree { .. }));
}

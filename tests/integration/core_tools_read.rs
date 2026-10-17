use jensen_harness_core::tools::{ToolError, Worktree, read};
use std::fs;
use tempfile::tempdir;

#[test]
fn reads_a_file_inside_the_worktree() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    assert_eq!(read::run(&worktree, "a.txt").unwrap(), "hello");
}

#[test]
fn refuses_a_path_outside_the_worktree() {
    let dir = tempdir().unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    let error = read::run(&worktree, "../a.txt").unwrap_err();
    assert!(
        matches!(error, ToolError::OutsideWorktree { .. }),
        "{error:?}"
    );
}

#[test]
fn refuses_an_absolute_path() {
    let dir = tempdir().unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    assert!(read::run(&worktree, "/etc/hosts").is_err());
}

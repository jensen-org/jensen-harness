use jensen_harness_core::tools::read;
use std::fs;
use tempfile::tempdir;

#[test]
fn reads_a_file_inside_the_worktree() {
    let worktree = tempdir().unwrap();
    fs::write(worktree.path().join("a.txt"), "hello").unwrap();
    assert_eq!(read::run(worktree.path(), "a.txt").unwrap(), "hello");
}

#[test]
fn refuses_a_path_outside_the_worktree() {
    let worktree = tempdir().unwrap();
    assert!(read::run(worktree.path(), "../a.txt").is_err());
}

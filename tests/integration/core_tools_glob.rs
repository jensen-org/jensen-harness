use jensen_harness_core::tools::glob;
use std::fs;
use tempfile::tempdir;

#[test]
fn lists_matching_files_relative_to_the_worktree() {
    let worktree = tempdir().unwrap();
    fs::write(worktree.path().join("a.rs"), "").unwrap();
    fs::write(worktree.path().join("b.txt"), "").unwrap();
    let matches = glob::run(worktree.path(), "*.rs").unwrap();
    assert_eq!(matches, vec!["a.rs".to_string()]);
}

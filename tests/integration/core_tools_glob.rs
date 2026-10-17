use jensen_harness_core::tools::{Worktree, glob};
use std::fs;
use tempfile::tempdir;

#[test]
fn lists_matching_files_relative_to_the_worktree() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.rs"), "").unwrap();
    fs::write(dir.path().join("b.txt"), "").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    let matches = glob::run(&worktree, "*.rs").unwrap();
    assert_eq!(matches, vec!["a.rs".to_string()]);
}

#[test]
fn a_star_stays_in_one_directory_and_a_double_star_recurses() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    fs::write(dir.path().join("top.rs"), "").unwrap();
    fs::write(dir.path().join("src/deep/x.rs"), "").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    assert_eq!(glob::run(&worktree, "*.rs").unwrap(), vec!["top.rs"]);
    assert_eq!(
        glob::run(&worktree, "**/*.rs").unwrap(),
        vec!["src/deep/x.rs", "top.rs"]
    );
}

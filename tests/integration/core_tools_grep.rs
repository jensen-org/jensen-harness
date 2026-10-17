use jensen_harness_core::tools::Worktree;
use jensen_harness_core::tools::grep::{self, Hit};
use std::fs;
use tempfile::tempdir;

#[test]
fn finds_matching_lines_with_their_line_number() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    let hits = grep::run(&worktree, "a.txt", "^t").unwrap();
    assert_eq!(
        hits,
        vec![
            Hit {
                path: "a.txt".to_string(),
                line_number: 2,
                line: "two".to_string()
            },
            Hit {
                path: "a.txt".to_string(),
                line_number: 3,
                line: "three".to_string()
            },
        ]
    );
}

#[test]
fn searches_a_directory_recursively_in_path_order() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/b.rs"), "needle\n").unwrap();
    fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    let paths: Vec<String> = grep::run(&worktree, ".", "needle")
        .unwrap()
        .into_iter()
        .map(|hit| hit.path)
        .collect();
    assert_eq!(paths, vec!["a.rs", "src/b.rs"]);
}

#[test]
fn skips_binary_files() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("blob.bin"), b"needle\0more needle").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();
    assert!(grep::run(&worktree, ".", "needle").unwrap().is_empty());
}

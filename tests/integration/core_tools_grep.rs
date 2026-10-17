use jensen_harness_core::tools::grep::{self, Hit};
use std::fs;
use tempfile::tempdir;

#[test]
fn finds_matching_lines_with_their_line_number() {
    let worktree = tempdir().unwrap();
    fs::write(worktree.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let hits = grep::run(worktree.path(), "a.txt", "^t").unwrap();
    assert_eq!(
        hits,
        vec![
            Hit {
                line_number: 2,
                line: "two".to_string()
            },
            Hit {
                line_number: 3,
                line: "three".to_string()
            },
        ]
    );
}

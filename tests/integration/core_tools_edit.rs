use jensen_harness_core::tools::{ToolError, Worktree, edit};
use std::fs;
use tempfile::tempdir;

fn open(dir: &tempfile::TempDir) -> Worktree {
    Worktree::open(dir.path()).unwrap()
}

#[test]
fn replaces_a_unique_match_and_reports_where() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello\nworld").unwrap();
    let outcome = edit::run(&open(&dir), "a.txt", "world", "there", false).unwrap();
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "hello\nthere"
    );
    assert_eq!(outcome.replacements, 1);
    assert_eq!(outcome.first_line, 2);
}

#[test]
fn a_repeated_match_is_refused_unless_replace_all_is_set() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "x x x").unwrap();
    let error = edit::run(&open(&dir), "a.txt", "x", "y", false).unwrap_err();
    assert!(
        matches!(error, ToolError::Ambiguous { count: 3, .. }),
        "{error:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "x x x"
    );

    let outcome = edit::run(&open(&dir), "a.txt", "x", "y", true).unwrap();
    assert_eq!(outcome.replacements, 3);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "y y y"
    );
}

#[test]
fn a_missing_or_empty_find_is_an_error() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "hello").unwrap();
    assert!(edit::run(&open(&dir), "a.txt", "absent", "y", false).is_err());
    assert!(edit::run(&open(&dir), "a.txt", "", "y", false).is_err());
}

#[test]
fn a_shorter_replacement_leaves_no_stale_tail() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), "abcdefgh").unwrap();
    edit::run(&open(&dir), "a.txt", "abcdefgh", "z", false).unwrap();
    assert_eq!(fs::read_to_string(dir.path().join("a.txt")).unwrap(), "z");
}

#[test]
fn refuses_to_touch_the_worktrees_own_git_pointer() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".git"), "gitdir: /real").unwrap();
    assert!(edit::run(&open(&dir), ".git", "real", "fake", false).is_err());
}

#[test]
fn refuses_a_git_pointer_edit_disguised_by_a_traversal_prefix() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".git"), "gitdir: /real").unwrap();
    fs::create_dir(dir.path().join("src")).unwrap();
    let error = edit::run(&open(&dir), "src/../.git", "real", "fake", false).unwrap_err();
    assert!(
        matches!(error, ToolError::OutsideWorktree { .. }),
        "{error:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".git")).unwrap(),
        "gitdir: /real"
    );
}

#[test]
fn refuses_a_path_outside_the_worktree() {
    let dir = tempdir().unwrap();
    assert!(edit::run(&open(&dir), "../a.txt", "x", "y", false).is_err());
}

#[cfg(unix)]
#[test]
fn refuses_a_hard_link_alias_of_the_worktrees_own_git_pointer() {
    let dir = tempdir().unwrap();
    let git_pointer = dir.path().join(".git");
    fs::write(&git_pointer, "gitdir: /real").unwrap();
    fs::hard_link(&git_pointer, dir.path().join("not-git-at-all.txt")).unwrap();

    let error = edit::run(&open(&dir), "not-git-at-all.txt", "real", "fake", false).unwrap_err();

    assert!(matches!(error, ToolError::OutsideWorktree { .. }));
    assert_eq!(fs::read_to_string(&git_pointer).unwrap(), "gitdir: /real");
}

#[cfg(unix)]
#[test]
fn refuses_any_file_with_more_than_one_hard_link_regardless_of_what_it_aliases() {
    let dir = tempdir().unwrap();
    let plain = dir.path().join("plain.txt");
    fs::write(&plain, "hello").unwrap();
    fs::hard_link(&plain, dir.path().join("alias.txt")).unwrap();

    let error = edit::run(&open(&dir), "alias.txt", "hello", "bye", false).unwrap_err();

    assert!(matches!(error, ToolError::OutsideWorktree { .. }));
}

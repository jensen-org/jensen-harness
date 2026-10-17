//! The escapes PR 1 closed one by one, now closed by the capability itself,
//! plus the new ones a tree wide walk opens.

use jensen_harness_core::tools::edit::open_nofollow;
use jensen_harness_core::tools::{ToolError, Worktree, edit, glob, grep, read};
use std::fs;
use std::path::Path;
use tempfile::tempdir;

#[cfg(unix)]
fn outside_with_secret() -> (tempfile::TempDir, std::path::PathBuf) {
    let outside = tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), "needle do not read me").unwrap();
    let path = outside.path().to_path_buf();
    (outside, path)
}

#[cfg(unix)]
#[test]
fn a_symlinked_file_pointing_outside_is_refused_by_read_edit_and_skipped_by_glob_and_grep() {
    let dir = tempdir().unwrap();
    let (_outside, outside_path) = outside_with_secret();
    std::os::unix::fs::symlink(outside_path.join("secret.txt"), dir.path().join("link.txt"))
        .unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    let error = read::run(&worktree, "link.txt").unwrap_err();
    assert!(
        matches!(error, ToolError::OutsideWorktree { .. }),
        "{error:?}"
    );
    assert!(edit::run(&worktree, "link.txt", "needle", "x", false).is_err());
    assert!(glob::run(&worktree, "*").unwrap().is_empty());
    assert!(grep::run(&worktree, ".", "needle").unwrap().is_empty());
    assert_eq!(
        fs::read_to_string(outside_path.join("secret.txt")).unwrap(),
        "needle do not read me"
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_pointing_outside_is_not_walked() {
    let dir = tempdir().unwrap();
    let (_outside, outside_path) = outside_with_secret();
    std::os::unix::fs::symlink(&outside_path, dir.path().join("linkdir")).unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    assert!(glob::run(&worktree, "**/*").unwrap().is_empty());
    assert!(grep::run(&worktree, ".", "needle").unwrap().is_empty());
    assert!(matches!(
        grep::run(&worktree, "linkdir", "needle").unwrap_err(),
        ToolError::OutsideWorktree { .. }
    ));
}

#[cfg(unix)]
#[test]
fn a_symlink_that_stays_inside_the_worktree_is_still_usable() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join("real.txt"), "hello").unwrap();
    std::os::unix::fs::symlink("real.txt", dir.path().join("alias.txt")).unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    assert_eq!(read::run(&worktree, "alias.txt").unwrap(), "hello");
    assert_eq!(
        glob::run(&worktree, "*.txt").unwrap(),
        vec!["alias.txt", "real.txt"]
    );
}

#[cfg(unix)]
#[test]
fn a_symlink_to_the_git_pointer_cannot_be_edited() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".git"), "gitdir: /real").unwrap();
    std::os::unix::fs::symlink(".git", dir.path().join("innocent.txt")).unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    let error = edit::run(&worktree, "innocent.txt", "real", "fake", false).unwrap_err();

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
fn gitignored_files_are_skipped_by_glob_and_grep_and_the_git_directory_never_listed() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".gitignore"), "target/\n").unwrap();
    fs::create_dir(dir.path().join("target")).unwrap();
    fs::write(dir.path().join("target/out.txt"), "needle").unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "needle").unwrap();
    fs::write(dir.path().join("kept.txt"), "needle").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    let listed = glob::run(&worktree, "**/*.txt").unwrap();
    assert_eq!(listed, vec!["kept.txt"]);
    let hits = grep::run(&worktree, ".", "needle").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].path, "kept.txt");
}

#[test]
fn a_dot_git_spelled_in_another_case_is_refused_on_every_filesystem() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    for spelling in [".GIT/config", ".Git/config", ".gIt/config"] {
        let error = open_nofollow(&worktree, Path::new(spelling)).unwrap_err();
        assert!(
            matches!(error, ToolError::OutsideWorktree { .. }),
            "{spelling}: {error:?}"
        );
        assert!(
            edit::run(&worktree, spelling, "core", "evil", false).is_err(),
            "{spelling}"
        );
    }
    assert_eq!(
        fs::read_to_string(dir.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[cfg(unix)]
#[test]
fn a_hard_link_to_the_git_pointer_is_refused_by_identity_not_by_name() {
    let dir = tempdir().unwrap();
    fs::write(dir.path().join(".git"), "gitdir: /real").unwrap();
    fs::hard_link(dir.path().join(".git"), dir.path().join("innocent.txt")).unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    let error = open_nofollow(&worktree, Path::new("innocent.txt")).unwrap_err();

    assert!(
        matches!(error, ToolError::OutsideWorktree { .. }),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn the_open_refuses_a_symlink_leaf_and_a_symlinked_directory_component() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join("sub")).unwrap();
    fs::write(dir.path().join("sub/x.txt"), "x").unwrap();
    std::os::unix::fs::symlink("sub/x.txt", dir.path().join("leaf-link")).unwrap();
    std::os::unix::fs::symlink("sub", dir.path().join("dir-link")).unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    assert!(open_nofollow(&worktree, Path::new("leaf-link")).is_err());
    assert!(open_nofollow(&worktree, Path::new("dir-link/x.txt")).is_err());
    assert!(open_nofollow(&worktree, Path::new("sub/x.txt")).is_ok());
}

#[cfg(unix)]
#[test]
fn an_edit_through_a_directory_symlink_to_git_is_refused() {
    let dir = tempdir().unwrap();
    fs::create_dir(dir.path().join(".git")).unwrap();
    fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
    std::os::unix::fs::symlink(".git", dir.path().join("alias")).unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    let error = edit::run(&worktree, "alias/config", "core", "evil", false).unwrap_err();

    assert!(
        matches!(error, ToolError::OutsideWorktree { .. }),
        "{error:?}"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join(".git/config")).unwrap(),
        "[core]\n"
    );
}

#[test]
fn an_ordinary_nested_edit_still_works() {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src/deep")).unwrap();
    fs::write(dir.path().join("src/deep/a.rs"), "let a = 1;").unwrap();
    let worktree = Worktree::open(dir.path()).unwrap();

    edit::run(&worktree, "src/deep/a.rs", "1", "2", false).unwrap();

    assert_eq!(
        fs::read_to_string(dir.path().join("src/deep/a.rs")).unwrap(),
        "let a = 2;"
    );
}

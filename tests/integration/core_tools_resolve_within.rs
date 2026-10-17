use jensen_harness_core::tools::{ToolError, resolve_within};

#[test]
fn resolve_within_accepts_a_relative_path_inside_the_worktree() {
    let worktree = tempfile::tempdir().unwrap();
    std::fs::create_dir(worktree.path().join("src")).unwrap();
    std::fs::write(worktree.path().join("src/lib.rs"), "").unwrap();
    let resolved = resolve_within(worktree.path(), "src/lib.rs").unwrap();
    assert_eq!(
        resolved,
        worktree.path().canonicalize().unwrap().join("src/lib.rs")
    );
}

#[test]
fn resolve_within_refuses_a_path_that_climbs_out_lexically() {
    let worktree = tempfile::tempdir().unwrap();
    let error = resolve_within(worktree.path(), "../secret.txt").unwrap_err();
    assert!(matches!(error, ToolError::OutsideWorktree { .. }));
}

#[cfg(unix)]
#[test]
fn resolve_within_refuses_a_symlink_that_escapes_the_worktree() {
    let worktree = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    std::fs::write(&secret, "do not read me").unwrap();
    std::os::unix::fs::symlink(&secret, worktree.path().join("innocent.txt")).unwrap();

    let error = resolve_within(worktree.path(), "innocent.txt").unwrap_err();

    assert!(
        matches!(error, ToolError::OutsideWorktree { .. }),
        "a symlink pointing outside the worktree must be refused, got {error:?}"
    );
}

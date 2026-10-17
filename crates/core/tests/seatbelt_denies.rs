//! Real, OS-enforced denial tests on macOS: these spawn the actual
//! `sandbox-exec` binary and prove it refuses the write, rather than
//! trusting the profile text alone. No exploit payload is ever constructed;
//! the redirect target in the `.git` test is a plain, inert string, exactly
//! what the regression tests for the same class of bug in Jensen's own
//! `writer_git.rs` do.

#![cfg(target_os = "macos")]
#![allow(clippy::unwrap_used)]

use jensen_harness_core::sandbox::SandboxPolicy;
use jensen_harness_core::sandbox::seatbelt::Seatbelt;
use jensen_harness_core::tools::shell;

#[tokio::test]
async fn a_write_outside_the_worktree_is_denied() {
    let worktree = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("pwned.txt");
    let policy = SandboxPolicy::confined_to(worktree.path());
    let command = format!("echo pwned > {}", target.display());

    let result = shell::run(&Seatbelt, &policy, &command).await.unwrap();

    assert_ne!(
        result.exit_code, 0,
        "the write must fail, stderr: {}",
        result.stderr
    );
    assert!(
        !target.exists(),
        "the file must never be created outside the worktree"
    );
}

#[tokio::test]
async fn a_write_to_the_worktrees_own_git_pointer_is_denied() {
    let worktree = tempfile::tempdir().unwrap();
    let git_pointer = worktree.path().join(".git");
    std::fs::write(&git_pointer, "gitdir: /real/place\n").unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());
    let command = format!("echo pwned > {}", git_pointer.display());

    let result = shell::run(&Seatbelt, &policy, &command).await.unwrap();

    assert_ne!(
        result.exit_code, 0,
        "the write must fail, stderr: {}",
        result.stderr
    );
    let after = std::fs::read_to_string(&git_pointer).unwrap();
    assert_eq!(
        after, "gitdir: /real/place\n",
        "the git pointer must be unchanged"
    );
}

#[tokio::test]
async fn a_write_inside_the_worktree_is_allowed() {
    let worktree = tempfile::tempdir().unwrap();
    let target = worktree.path().join("a.txt");
    let policy = SandboxPolicy::confined_to(worktree.path());
    let command = format!("echo hello > {}", target.display());

    let result = shell::run(&Seatbelt, &policy, &command).await.unwrap();

    assert_eq!(
        result.exit_code, 0,
        "a write inside the worktree must be allowed, stderr: {}",
        result.stderr
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap().trim(), "hello");
}

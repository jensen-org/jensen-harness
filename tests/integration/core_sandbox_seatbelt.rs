#![cfg(target_os = "macos")]

use jensen_harness_core::sandbox::seatbelt::{Seatbelt, profile};
use jensen_harness_core::sandbox::{Sandbox, SandboxPolicy};

#[test]
fn the_profile_denies_the_worktrees_own_git_pointer_after_allowing_the_worktree() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let text = profile(&policy);
    let worktree_allow = text
        .find("allow file-write* (subpath \"/repo/task\")")
        .unwrap();
    let git_deny = text
        .find("deny file-write* (subpath \"/repo/task/.git\")")
        .unwrap();
    assert!(
        worktree_allow < git_deny,
        "the git deny must come after the worktree allow so it wins"
    );
}

#[test]
fn the_git_denys_resolved_prefix_matches_the_worktrees_resolved_prefix() {
    // A real, existing worktree under the system temp root, so this proves
    // the fix for a real /var -> /private/var style symlink, not just the
    // no-op fallback path a nonexistent worktree exercises.
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());
    let text = profile(&policy);
    let canonical_worktree = worktree.path().canonicalize().unwrap();
    let canonical_worktree = canonical_worktree.display().to_string();
    let worktree_allow = text
        .find(&format!(
            "allow file-write* (subpath \"{canonical_worktree}\")"
        ))
        .expect("the worktree allow rule must use the canonical worktree path");
    let git_deny = text
        .find(&format!(
            "deny file-write* (subpath \"{canonical_worktree}/.git\")"
        ))
        .expect("the git deny rule must be nested under the same canonical prefix");
    assert!(worktree_allow < git_deny);
}

#[test]
fn confine_wraps_the_program_with_sandbox_exec() {
    let policy = SandboxPolicy::confined_to("/repo/task");
    let confined = Seatbelt
        .confine(&policy, "/bin/sh", &["-c".to_string(), "true".to_string()])
        .unwrap();
    assert_eq!(confined.program, "sandbox-exec");
    assert_eq!(confined.args.last(), Some(&"true".to_string()));
    assert_eq!(confined.report.backend, "seatbelt");
}

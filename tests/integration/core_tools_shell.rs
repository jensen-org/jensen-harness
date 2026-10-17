// This file needs std::env::set_var/remove_var to simulate a daemon
// process holding a secret in its own environment, exactly the hazard the
// fix under test guards against; nowhere else in this crate uses them,
// and this file's one test owns the variable name it sets.
#![allow(clippy::disallowed_methods)]

use crate::common::NoopSandbox;
use jensen_harness_core::sandbox::SandboxPolicy;
use jensen_harness_core::tools::shell;

#[tokio::test]
async fn the_daemons_own_secret_does_not_reach_the_shell_command() {
    // SAFETY: this test owns the variable name and no other test in this
    // binary reads or writes it; set, not left for a real process-wide
    // mutation concern, since this models exactly what a real daemon
    // holding a provider API key looks like in its own environment before
    // a tool call runs.
    unsafe { std::env::set_var("JENSEN_HARNESS_TEST_SECRET_7F3A", "super-secret-value") };
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());

    let result = shell::run(
        &NoopSandbox,
        &policy,
        "echo \"got:$JENSEN_HARNESS_TEST_SECRET_7F3A\"",
    )
    .await
    .unwrap();

    // SAFETY: see above.
    unsafe { std::env::remove_var("JENSEN_HARNESS_TEST_SECRET_7F3A") };
    assert_eq!(
        result.stdout.trim(),
        "got:",
        "the daemon's secret must not reach the child"
    );
}

#[tokio::test]
async fn the_cleared_environment_still_has_a_usable_path() {
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());

    let result = shell::run(&NoopSandbox, &policy, "true").await.unwrap();

    assert_eq!(
        result.exit_code, 0,
        "a trivial command must still run with the allowlisted env"
    );
}

#[tokio::test]
async fn the_scratch_directory_exists_and_is_writable_for_the_command() {
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());

    let result = shell::run(
        &NoopSandbox,
        &policy,
        "echo ok > \"$TMPDIR/probe\" && cat \"$TMPDIR/probe\"",
    )
    .await
    .unwrap();

    assert_eq!(result.stdout.trim(), "ok");
    assert!(policy.scratch.join("probe").exists());
    std::fs::remove_dir_all(&policy.scratch).unwrap();
}

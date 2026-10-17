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

#[tokio::test]
async fn a_command_that_outlives_its_timeout_is_killed_and_reported() {
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());

    let started = std::time::Instant::now();
    let result = shell::run_with_timeout(
        &NoopSandbox,
        &policy,
        "sleep 30",
        std::time::Duration::from_millis(200),
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 124);
    assert!(result.stderr.contains("timed out"));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
}

#[cfg(unix)]
fn process_is_gone(pid: &str) -> bool {
    for _ in 0..50 {
        let alive = std::process::Command::new("kill")
            .args(["-0", pid])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        if !alive {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    false
}

#[cfg(unix)]
#[tokio::test]
async fn a_background_process_does_not_outlive_the_call() {
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());

    let result = shell::run(
        &NoopSandbox,
        &policy,
        "(sleep 300 >/dev/null 2>&1 & echo $! > \"$TMPDIR/pid\")",
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 0);
    let pid = std::fs::read_to_string(policy.scratch.join("pid")).unwrap();
    assert!(process_is_gone(pid.trim()), "pid {pid} survived the call");
    std::fs::remove_dir_all(&policy.scratch).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn a_timeout_kills_the_whole_group_not_just_the_shell() {
    let worktree = tempfile::tempdir().unwrap();
    let policy = SandboxPolicy::confined_to(worktree.path());

    let result = shell::run_with_timeout(
        &NoopSandbox,
        &policy,
        "sleep 300 & echo $! > \"$TMPDIR/pid\"; wait",
        std::time::Duration::from_millis(400),
    )
    .await
    .unwrap();

    assert_eq!(result.exit_code, 124);
    let pid = std::fs::read_to_string(policy.scratch.join("pid")).unwrap();
    assert!(
        process_is_gone(pid.trim()),
        "pid {pid} survived the timeout"
    );
    std::fs::remove_dir_all(&policy.scratch).unwrap();
}

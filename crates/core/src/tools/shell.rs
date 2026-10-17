//! The one tool that runs an arbitrary command, so it is the one tool
//! confined by a real [`Sandbox`] backend rather than an in-process path
//! check: no static analysis of a shell command string can vet what it will
//! do.

use std::time::Duration;

use super::ToolError;
use crate::sandbox::{Sandbox, SandboxPolicy};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
const TIMED_OUT_EXIT_CODE: i32 = 124;

#[derive(Debug, Clone)]
pub struct ShellResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// # Errors
/// [`ToolError::Sandbox`] if the policy could not be applied,
/// [`ToolError::Io`] if the confined process could not be started.
pub async fn run(
    sandbox: &dyn Sandbox,
    policy: &SandboxPolicy,
    command: &str,
) -> Result<ShellResult, ToolError> {
    run_with_timeout(sandbox, policy, command, DEFAULT_TIMEOUT).await
}

/// A command that outlives `timeout` is killed and reported with exit code
/// 124, so a hung process cannot stall the run.
///
/// # Errors
/// [`ToolError::Sandbox`] if the policy could not be applied,
/// [`ToolError::Io`] if the confined process could not be started.
pub async fn run_with_timeout(
    sandbox: &dyn Sandbox,
    policy: &SandboxPolicy,
    command: &str,
    timeout: Duration,
) -> Result<ShellResult, ToolError> {
    tokio::fs::create_dir_all(&policy.scratch)
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    let confined = sandbox
        .confine(policy, "/bin/sh", &["-c".to_string(), command.to_string()])
        .map_err(|error| ToolError::Sandbox(error.to_string()))?;
    // The sandbox confines the filesystem and the network, but a process's
    // environment is inherited regardless, so without clearing it a shell
    // command would otherwise see this daemon's own environment, including
    // ANTHROPIC_API_KEY and any other secret a provider reads from it. Only
    // a small, host-derived allowlist is passed back in; none of it is a
    // secret, and a host that does not define one of these at all gets a
    // reasonable, well known default rather than an empty value.
    let mut child = tokio::process::Command::new(&confined.program);
    child
        .args(&confined.args)
        .current_dir(&policy.worktree)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_unset| "/usr/bin:/bin".to_string()),
        )
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_unset| "/".to_string()),
        )
        .env(
            "LANG",
            std::env::var("LANG").unwrap_or_else(|_unset| "C".to_string()),
        )
        .env("TMPDIR", &policy.scratch)
        .kill_on_drop(true);
    let output = match tokio::time::timeout(timeout, child.output()).await {
        Ok(finished) => finished.map_err(|error| ToolError::Io(error.to_string()))?,
        Err(_elapsed) => {
            return Ok(ShellResult {
                exit_code: TIMED_OUT_EXIT_CODE,
                stdout: String::new(),
                stderr: format!("timed out after {}s", timeout.as_secs()),
            });
        }
    };
    Ok(ShellResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

//! The one tool that runs an arbitrary command, so it is the one tool
//! confined by a real [`Sandbox`] backend rather than an in-process path
//! check: no static analysis of a shell command string can vet what it will
//! do.

use super::ToolError;
use crate::sandbox::{Sandbox, SandboxPolicy};

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
    let confined = sandbox
        .confine(policy, "/bin/sh", &["-c".to_string(), command.to_string()])
        .map_err(|error| ToolError::Sandbox(error.to_string()))?;
    let output = tokio::process::Command::new(&confined.program)
        .args(&confined.args)
        .current_dir(&policy.worktree)
        .output()
        .await
        .map_err(|error| ToolError::Io(error.to_string()))?;
    Ok(ShellResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

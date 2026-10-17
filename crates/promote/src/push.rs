use std::path::Path;
use std::process::Stdio;

use base64::Engine as _;

use crate::error::PromoteError;
use crate::forge::{ForgeKind, ForgeTarget};

const NULL_DEVICE: &str = "/dev/null";

pub async fn push(
    repo: &Path,
    remote_url: &str,
    local_ref: &str,
    remote_branch: &str,
    target: &ForgeTarget,
) -> Result<(), PromoteError> {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(repo)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
        )
        .env("HOME", repo)
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_SYSTEM", NULL_DEVICE)
        .env("GIT_TERMINAL_PROMPT", "0");
    if let Ok(socket) = std::env::var("SSH_AUTH_SOCK") {
        command.env("SSH_AUTH_SOCK", socket);
    }
    let mut count = 2;
    command
        .env("GIT_CONFIG_KEY_0", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_0", NULL_DEVICE)
        .env("GIT_CONFIG_KEY_1", "core.fsmonitor")
        .env("GIT_CONFIG_VALUE_1", "false");
    if remote_url.starts_with("http://") || remote_url.starts_with("https://") {
        let user = match target.kind {
            ForgeKind::GitHub => "x-access-token",
            ForgeKind::GitLab => "oauth2",
        };
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("{user}:{}", target.token.expose()));
        command.env("GIT_CONFIG_KEY_2", "http.extraHeader").env(
            "GIT_CONFIG_VALUE_2",
            format!("Authorization: Basic {credentials}"),
        );
        count = 3;
    }
    let output = command
        .env("GIT_CONFIG_COUNT", count.to_string())
        .arg("push")
        .arg("--porcelain")
        .arg(remote_url)
        .arg(format!("{local_ref}:refs/heads/{remote_branch}"))
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|error| PromoteError::Push(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(PromoteError::Push(target.token.redact(stderr.trim())))
}

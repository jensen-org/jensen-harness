use std::path::Path;
use std::process::Stdio;

use base64::Engine as _;

use crate::error::PromoteError;
use crate::forge::{ForgeKind, ForgeTarget};

const NULL_DEVICE: &str = "/dev/null";

fn git(scratch: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new("git");
    command
        .current_dir(scratch)
        .env_clear()
        .env(
            "PATH",
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
        )
        .env("HOME", scratch)
        .env("GIT_DIR", scratch)
        .env("GIT_CONFIG_GLOBAL", NULL_DEVICE)
        .env("GIT_CONFIG_SYSTEM", NULL_DEVICE)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null());
    if let Ok(socket) = std::env::var("SSH_AUTH_SOCK") {
        command.env("SSH_AUTH_SOCK", socket);
    }
    command
}

async fn run(
    mut command: tokio::process::Command,
    target: &ForgeTarget,
) -> Result<(), PromoteError> {
    let output = command
        .output()
        .await
        .map_err(|error| PromoteError::Push(error.to_string()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(PromoteError::Push(target.token.redact(stderr.trim())))
}

fn host_of(url: &str) -> Option<(String, String)> {
    let parsed = url::Url::parse(url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    let origin = match parsed.port() {
        Some(port) => format!("{}://{host}:{port}/", parsed.scheme()),
        None => format!("{}://{host}/", parsed.scheme()),
    };
    Some((host, origin))
}

pub async fn push(
    objects_dir: &Path,
    commit: &str,
    remote_url: &str,
    remote_branch: &str,
    target: &ForgeTarget,
) -> Result<(), PromoteError> {
    let scratch = tempfile::tempdir().map_err(|error| PromoteError::Io(error.to_string()))?;
    let dir = scratch.path();
    run(
        {
            let mut command = git(dir);
            command.args(["init", "-q", "--bare", "."]);
            command
        },
        target,
    )
    .await?;
    let alternates = dir.join("objects").join("info");
    std::fs::create_dir_all(&alternates).map_err(|error| PromoteError::Io(error.to_string()))?;
    std::fs::write(
        alternates.join("alternates"),
        format!("{}\n", objects_dir.display()),
    )
    .map_err(|error| PromoteError::Io(error.to_string()))?;
    run(
        {
            let mut command = git(dir);
            command.args(["update-ref", "refs/heads/push", commit]);
            command
        },
        target,
    )
    .await?;
    let mut command = git(dir);
    let mut count = 0;
    if let Some((host, origin)) = host_of(remote_url)
        && host == target.host
    {
        let user = match target.kind {
            ForgeKind::GitHub => "x-access-token",
            ForgeKind::GitLab => "oauth2",
        };
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("{user}:{}", target.token.expose()));
        command
            .env("GIT_CONFIG_KEY_0", format!("http.{origin}.extraHeader"))
            .env(
                "GIT_CONFIG_VALUE_0",
                format!("Authorization: Basic {credentials}"),
            );
        count = 1;
    }
    command
        .env("GIT_CONFIG_COUNT", count.to_string())
        .args(["push", "--porcelain"])
        .arg(remote_url)
        .arg(format!("refs/heads/push:refs/heads/{remote_branch}"));
    run(command, target).await
}

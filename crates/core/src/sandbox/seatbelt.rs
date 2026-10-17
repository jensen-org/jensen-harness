//! macOS confinement via `sandbox-exec`. Reads are allowed everywhere;
//! writes are allowed only where a rule says so. Rules are emitted in order,
//! so a later, more specific `deny` (the worktree's own `.git`) overrides an
//! earlier, broader `allow` (the worktree itself), matching the ordered
//! rule-list design every backend here shares.

use std::path::{Path, PathBuf};

use super::{
    Confined, EnforcementReport, FsMode, NetworkPolicy, Sandbox, SandboxError, SandboxPolicy,
};

pub struct Seatbelt;

fn quote(path: &Path) -> String {
    path.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Seatbelt's `subpath` matches the literal, symlink-resolved path, while
/// `/var` (and so `std::env::temp_dir()`, and any path under it) is itself a
/// symlink to `/private/var` on macOS. A rule built from the unresolved path
/// would then silently fail to match what the kernel actually opens, so
/// every path that reaches the profile is canonicalized first. A path that
/// does not exist yet is used as given, since there is nothing to resolve.
fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_not_found| path.to_path_buf())
}

fn profile(policy: &SandboxPolicy) -> String {
    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow process-fork)".to_string(),
        "(allow process-exec)".to_string(),
        "(allow file-read*)".to_string(),
        "(allow signal (target self))".to_string(),
        format!(
            "(allow file-write* (subpath \"{}\"))",
            quote(&canonical(&policy.worktree))
        ),
        format!(
            "(allow file-write* (subpath \"{}\"))",
            quote(&canonical(&policy.scratch))
        ),
    ];
    for rule in &policy.extra_rules {
        let subpath = format!("(subpath \"{}\")", quote(&canonical(&rule.path)));
        match rule.mode {
            FsMode::Deny => lines.push(format!("(deny file-write* {subpath})")),
            FsMode::ReadOnly => lines.push(format!("(deny file-write* {subpath})")),
            FsMode::ReadWrite => lines.push(format!("(allow file-write* {subpath})")),
        }
    }
    match policy.network {
        NetworkPolicy::None => {}
        NetworkPolicy::ProxyAllowlist { port } => {
            lines.push(format!(
                "(allow network-outbound (remote ip \"localhost:{port}\"))"
            ));
        }
        NetworkPolicy::Full => lines.push("(allow network*)".to_string()),
    }
    lines.join("\n")
}

impl Sandbox for Seatbelt {
    fn confine(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[String],
    ) -> Result<Confined, SandboxError> {
        std::fs::create_dir_all(&policy.scratch).map_err(|error| {
            SandboxError::Policy(format!(
                "the scratch directory could not be created: {error}"
            ))
        })?;
        let profile = profile(policy);
        let mut wrapped = vec![
            "-p".to_string(),
            profile,
            "--".to_string(),
            program.to_string(),
        ];
        wrapped.extend(args.iter().cloned());
        let network_tier = match policy.network {
            NetworkPolicy::None => "none",
            NetworkPolicy::ProxyAllowlist { .. } => "proxy_allowlist",
            NetworkPolicy::Full => "full",
        };
        Ok(Confined {
            program: "sandbox-exec".to_string(),
            args: wrapped,
            report: EnforcementReport {
                backend: "seatbelt",
                filesystem_rules_applied: 2 + policy.extra_rules.len(),
                network_tier,
                unsupported: Vec::new(),
            },
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{Seatbelt, profile};
    use crate::sandbox::{Sandbox, SandboxPolicy};

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
    fn confine_wraps_the_program_with_sandbox_exec() {
        let policy = SandboxPolicy::confined_to("/repo/task");
        let confined = Seatbelt
            .confine(&policy, "/bin/sh", &["-c".to_string(), "true".to_string()])
            .unwrap();
        assert_eq!(confined.program, "sandbox-exec");
        assert_eq!(confined.args.last(), Some(&"true".to_string()));
        assert_eq!(confined.report.backend, "seatbelt");
    }
}

//! Linux confinement via `bwrap`. The root filesystem is bound read only,
//! the worktree is bound read-write, and extra rules are applied last as
//! nested mounts on top of that, the same most-specific-wins ordering the
//! Seatbelt backend uses, because bubblewrap applies `--bind`/`--ro-bind`
//! arguments in the order given and a later mount on a subpath overrides an
//! earlier one.
//!
//! Landlock is deliberately not used here: it is strictly additive up the
//! ancestor chain and cannot carve a deny-hole (the worktree's own `.git`)
//! out of an otherwise writable tree, which is exactly what this backend
//! needs. It stays a documented future fallback, not the primary mechanism.
//!
//! Ubuntu 24.04 restricts unprivileged user namespaces with AppArmor by
//! default, which breaks bubblewrap outright unless a compensating profile
//! is loaded. [`check_user_namespaces`] detects this so the caller can fail
//! loudly instead of believing it is sandboxed when it is not.

use std::process::Command;

use super::{
    Confined, EnforcementReport, FsMode, NetworkPolicy, Sandbox, SandboxError, SandboxPolicy,
};

pub struct Bubblewrap;

/// # Errors
/// Returns [`SandboxError::Unavailable`] when this host cannot create an
/// unprivileged user namespace, the precondition every bubblewrap sandbox
/// needs, most commonly because Ubuntu's AppArmor restriction (default
/// since 24.04) is active and no compensating profile is loaded.
pub fn check_user_namespaces() -> Result<(), SandboxError> {
    let probe = Command::new("bwrap")
        .args(["--unshare-user", "--", "/bin/true"])
        .output()
        .map_err(|error| SandboxError::Unavailable(format!("bwrap could not run: {error}")))?;
    if probe.status.success() {
        return Ok(());
    }
    Err(SandboxError::Unavailable(
        "this host refused an unprivileged user namespace, most likely Ubuntu's AppArmor \
         restriction on unprivileged user namespaces (default since 24.04); load a \
         compensating AppArmor profile for bwrap before relying on this sandbox"
            .to_string(),
    ))
}

impl Sandbox for Bubblewrap {
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
        let worktree = policy.worktree.to_string_lossy().into_owned();
        let tmp = policy.scratch.to_string_lossy().into_owned();
        let mut wrapped = vec![
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--bind".to_string(),
            worktree.clone(),
            worktree,
            "--bind".to_string(),
            tmp.clone(),
            tmp,
            "--die-with-parent".to_string(),
        ];
        let mut unsupported = Vec::new();
        for rule in &policy.extra_rules {
            let path = rule.path.to_string_lossy().into_owned();
            match rule.mode {
                FsMode::Deny | FsMode::ReadOnly => {
                    wrapped.push("--ro-bind".to_string());
                    wrapped.push(path.clone());
                    wrapped.push(path);
                }
                FsMode::ReadWrite => {
                    wrapped.push("--bind".to_string());
                    wrapped.push(path.clone());
                    wrapped.push(path);
                }
            }
        }
        let network_tier = match policy.network {
            NetworkPolicy::None => {
                wrapped.push("--unshare-net".to_string());
                "none"
            }
            NetworkPolicy::ProxyAllowlist { .. } => {
                unsupported.push(
                    "a proxy allowlist is not yet enforced by this backend, only full or no \
                     network"
                        .to_string(),
                );
                "full"
            }
            NetworkPolicy::Full => "full",
        };
        wrapped.push("--".to_string());
        wrapped.push(program.to_string());
        wrapped.extend(args.iter().cloned());
        Ok(Confined {
            program: "bwrap".to_string(),
            args: wrapped,
            report: EnforcementReport {
                backend: "bubblewrap",
                filesystem_rules_applied: 3 + policy.extra_rules.len(),
                network_tier,
                unsupported,
            },
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::Bubblewrap;
    use crate::sandbox::{Sandbox, SandboxPolicy};

    #[test]
    fn confine_binds_the_git_pointer_read_only_after_the_worktree_bind() {
        let policy = SandboxPolicy::confined_to("/repo/task");
        let confined = Bubblewrap
            .confine(&policy, "/bin/sh", &["-c".to_string(), "true".to_string()])
            .unwrap();
        let worktree_bind = confined
            .args
            .iter()
            .position(|arg| arg == "/repo/task")
            .unwrap();
        let git_ro_bind = confined
            .args
            .iter()
            .position(|arg| arg == "/repo/task/.git")
            .unwrap();
        assert!(
            worktree_bind < git_ro_bind,
            "the read-only .git bind must come after the read-write worktree bind so it wins"
        );
        assert_eq!(
            confined.args.get(git_ro_bind - 1),
            Some(&"--ro-bind".to_string())
        );
    }

    #[test]
    fn confine_unshares_the_network_by_default() {
        let policy = SandboxPolicy::confined_to("/repo/task");
        let confined = Bubblewrap.confine(&policy, "/bin/true", &[]).unwrap();
        assert!(confined.args.iter().any(|arg| arg == "--unshare-net"));
        assert_eq!(confined.report.network_tier, "none");
    }
}

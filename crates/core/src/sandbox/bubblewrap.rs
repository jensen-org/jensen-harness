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
            // A read-only bind of / still leaves every filesystem-path Unix
            // domain socket under it connectable (read-only blocks writes
            // to the inode, not AF_UNIX connect), so /run, /var/run and
            // /tmp, the usual home of a container runtime's or a display
            // server's socket, are masked with a private tmpfs before
            // anything more specific is bound back on top of them. This
            // only reaches filesystem-path sockets, and only takes effect
            // at all under NetworkPolicy::None, which also passes
            // --unshare-net below: a fresh network namespace is what
            // isolates Linux's abstract-namespace AF_UNIX sockets (those
            // have no filesystem path at all, so no amount of masking
            // reaches them) and the host's other network-namespaced state.
            // NetworkPolicy::Full deliberately keeps the host's network
            // namespace, which is what "full" means, so under Full neither
            // this masking nor the namespace isolation applies: abstract
            // sockets and other host runtime sockets remain reachable
            // there, and that is a property of asking for full network
            // access, not a gap in this backend.
            "--tmpfs".to_string(),
            "/tmp".to_string(),
            "--tmpfs".to_string(),
            "/run".to_string(),
            "--tmpfs".to_string(),
            "/var/run".to_string(),
            "--bind".to_string(),
            worktree.clone(),
            worktree,
            "--bind".to_string(),
            tmp.clone(),
            tmp,
            "--unshare-pid".to_string(),
            "--unshare-ipc".to_string(),
            "--new-session".to_string(),
            "--die-with-parent".to_string(),
        ];
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
        // A policy this backend cannot satisfy is refused outright rather
        // than downgraded to something broader: granting full network
        // access in place of an unimplemented proxy allowlist would be the
        // opposite of what was asked, and a caller that assumed the
        // narrower policy took effect would never know otherwise.
        let network_tier = match policy.network {
            NetworkPolicy::None => {
                wrapped.push("--unshare-net".to_string());
                "none"
            }
            NetworkPolicy::ProxyAllowlist { .. } => {
                return Err(SandboxError::Policy(
                    "a proxy allowlist is not yet enforced by the bubblewrap backend, and it \
                     refuses to substitute full network access for it"
                        .to_string(),
                ));
            }
            NetworkPolicy::Full => "full",
        };
        let unsupported = if matches!(policy.network, NetworkPolicy::Full) {
            vec![
                "Full keeps the host's network namespace, so abstract-namespace AF_UNIX \
                 sockets and other host runtime sockets stay reachable; this is what full \
                 network access means, not a gap this backend tries to close"
                    .to_string(),
            ]
        } else {
            Vec::new()
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

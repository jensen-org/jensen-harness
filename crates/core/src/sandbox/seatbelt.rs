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

/// Rewrites `path`, which was built against `original_root` (typically
/// `worktree.join(".git")`), onto `canonical_root`. This keeps an extra
/// rule's resolved prefix identical to the worktree's own resolved prefix
/// even when `path` itself does not exist yet to canonicalize directly: a
/// deny rule built from a path that resolved independently, and possibly
/// inconsistently, would risk not actually nesting under the worktree's own
/// resolved allow rule, silently failing to override it.
fn rebase(original_root: &Path, canonical_root: &Path, path: &Path) -> PathBuf {
    match path.strip_prefix(original_root) {
        Ok(suffix) => canonical_root.join(suffix),
        Err(_not_nested) => canonical(path),
    }
}

/// Builds the Seatbelt profile text `confine` wraps a command with. Public
/// so tests can drive it directly rather than reaching into this module:
/// a pure helper becomes part of the crate's tested surface, not a private
/// implementation detail a test module gets special access to.
#[must_use]
pub fn profile(policy: &SandboxPolicy) -> String {
    let worktree = canonical(&policy.worktree);
    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow process-fork)".to_string(),
        "(allow process-exec)".to_string(),
        "(allow file-read*)".to_string(),
        "(allow sysctl-read)".to_string(),
        "(allow signal (target self))".to_string(),
        format!("(allow file-write* (subpath \"{}\"))", quote(&worktree)),
        format!(
            "(allow file-write* (subpath \"{}\"))",
            quote(&canonical(&policy.scratch))
        ),
    ];
    for rule in &policy.extra_rules {
        let resolved = rebase(&policy.worktree, &worktree, &rule.path);
        let subpath = format!("(subpath \"{}\")", quote(&resolved));
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

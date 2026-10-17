//! A `Sandbox` confines a tool's process to a task worktree before it ever
//! runs, so execution needs no human approval: the boundary is enforced by
//! the operating system, not by asking.
//!
//! Filesystem policy is an ordered, most-specific-wins rule list rather than
//! a flat allow set, because that is the one shape every backend (Seatbelt,
//! bubblewrap, and eventually Windows) can express; a flat allow set cannot
//! carve a deny-hole (such as a worktree's own `.git`) out of an otherwise
//! writable tree. Network policy is a three state enum, the common
//! denominator across backends.

use std::path::PathBuf;

// Both modules compile on every platform: building the argument list a
// backend would run is pure, portable code with no OS-specific syscall
// beyond generic filesystem operations, so each backend's own tests can
// run and be verified regardless of which OS is building. Only actually
// running the resulting command (sandbox-exec, bwrap) needs the matching
// OS, which is why those specific tests are separately platform-gated.
pub mod bubblewrap;
pub mod seatbelt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsMode {
    ReadWrite,
    ReadOnly,
    Deny,
}

/// One filesystem rule. Rules are applied in order; a later rule whose path
/// is equal to or nested under an earlier one overrides it for that subtree.
#[derive(Debug, Clone)]
pub struct FsRule {
    pub path: PathBuf,
    pub mode: FsMode,
}

impl FsRule {
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, mode: FsMode) -> Self {
        Self {
            path: path.into(),
            mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    None,
    /// Outbound traffic only to a proxy on localhost at this port.
    ProxyAllowlist {
        port: u16,
    },
    Full,
}

/// What a tool call is confined to. The worktree is read-write by default;
/// `extra_rules` are applied after it, so the worktree's own `.git` is
/// denied even though the worktree itself is writable.
#[derive(Debug, Clone)]
pub struct SandboxPolicy {
    pub worktree: PathBuf,
    /// A directory dedicated to this one sandbox, not the shared OS temp
    /// root: the whole system temp directory is a shared space other
    /// processes' own temp files live in too, so allowing writes to all of
    /// it would undo the worktree confinement for anything that happens to
    /// land there.
    pub scratch: PathBuf,
    pub extra_rules: Vec<FsRule>,
    pub network: NetworkPolicy,
}

impl SandboxPolicy {
    /// The default posture for a writer node: the worktree is writable, its
    /// own git metadata is not, a scratch directory unique to this sandbox
    /// is writable, everything else is read only, and the network is
    /// closed.
    #[must_use]
    pub fn confined_to(worktree: impl Into<PathBuf>) -> Self {
        let worktree = worktree.into();
        let git_pointer = worktree.join(".git");
        let scratch = std::env::temp_dir().join(format!("jensen-harness-{}", uuid::Uuid::new_v4()));
        Self {
            worktree,
            scratch,
            extra_rules: vec![FsRule::new(git_pointer, FsMode::Deny)],
            network: NetworkPolicy::None,
        }
    }
}

/// What a backend actually managed to enforce, so a caller never has to
/// assume a request was fully honored. A backend that cannot satisfy part
/// of a policy reports it here rather than silently under-enforcing.
#[derive(Debug, Clone)]
pub struct EnforcementReport {
    pub backend: &'static str,
    pub filesystem_rules_applied: usize,
    pub network_tier: &'static str,
    pub unsupported: Vec<String>,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SandboxError {
    #[error("the sandbox backend for this platform is not available: {0}")]
    Unavailable(String),
    #[error("the sandbox policy could not be prepared: {0}")]
    Policy(String),
}

/// A sandboxed, ready-to-run command: the program and its arguments after a
/// backend has wrapped them, plus the report of what the wrapping actually
/// enforces.
#[derive(Debug)]
pub struct Confined {
    pub program: String,
    pub args: Vec<String>,
    pub report: EnforcementReport,
}

pub trait Sandbox: Send + Sync {
    /// Wrap `program`/`args` so running the result enforces `policy`.
    ///
    /// # Errors
    /// Returns [`SandboxError`] when the backend is unavailable on this host
    /// or the policy cannot be expressed.
    fn confine(
        &self,
        policy: &SandboxPolicy,
        program: &str,
        args: &[String],
    ) -> Result<Confined, SandboxError>;
}

/// The backend this platform ships today. Returns
/// [`SandboxError::Unavailable`] on a platform with no backend yet (for
/// example Windows in this phase), so a caller never silently runs
/// unsandboxed.
#[must_use]
pub fn platform_default() -> Box<dyn Sandbox> {
    #[cfg(target_os = "macos")]
    {
        Box::new(seatbelt::Seatbelt)
    }
    #[cfg(target_os = "linux")]
    {
        Box::new(bubblewrap::Bubblewrap)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Box::new(Unavailable)
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
struct Unavailable;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl Sandbox for Unavailable {
    fn confine(
        &self,
        _policy: &SandboxPolicy,
        _program: &str,
        _args: &[String],
    ) -> Result<Confined, SandboxError> {
        Err(SandboxError::Unavailable(
            "no sandbox backend ships for this platform yet".to_string(),
        ))
    }
}

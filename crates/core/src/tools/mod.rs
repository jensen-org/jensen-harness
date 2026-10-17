//! The 5 built-in tools. `read`, `edit`, `glob` and `grep` touch the
//! filesystem directly and enforce confinement themselves, by rejecting any
//! path that resolves outside the worktree: a fixed, enumerable path check
//! is enough for them, since their operations are not arbitrary code.
//! `shell` runs an arbitrary command, which no static check can vet, so it
//! is the one tool that goes through a real [`Sandbox`](crate::sandbox::Sandbox)
//! backend, enforced by the operating system rather than by this crate.

pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod shell;

use std::path::{Path, PathBuf};

use crate::sandbox::is_within;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ToolError {
    #[error("{path} is outside the worktree, so this tool refuses it")]
    OutsideWorktree { path: String },
    #[error("the path could not be read or written: {0}")]
    Io(String),
    #[error("the sandbox refused this command: {0}")]
    Sandbox(String),
}

/// Resolves `relative` against `worktree` and refuses it if the result
/// escapes the worktree, the same containment rule the sandbox backends
/// enforce for `shell`, applied here in code for tools that touch files
/// directly instead of spawning a process.
pub fn resolve_within(worktree: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let candidate = worktree.join(relative);
    if is_within(worktree, &candidate) {
        Ok(candidate)
    } else {
        Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ToolError, resolve_within};
    use std::path::Path;

    #[test]
    fn resolve_within_accepts_a_relative_path_inside_the_worktree() {
        let resolved = resolve_within(Path::new("/repo/task"), "src/lib.rs").unwrap();
        assert_eq!(resolved, Path::new("/repo/task/src/lib.rs"));
    }

    #[test]
    fn resolve_within_refuses_a_path_that_climbs_out() {
        let error = resolve_within(Path::new("/repo/task"), "../secret.txt").unwrap_err();
        assert!(matches!(error, ToolError::OutsideWorktree { .. }));
    }
}

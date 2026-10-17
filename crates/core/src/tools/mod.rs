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
///
/// A lexical check alone is not enough: a symlink planted inside the
/// worktree, pointing outside it, would pass a purely textual containment
/// check while still reading or writing wherever the link actually points.
/// Once the lexical check accepts a path, the target is required to exist
/// (every caller here only ever reads, edits or lists real files) and its
/// fully resolved form is checked against the worktree's own resolved form,
/// so a symlink escape is caught the same way the lexical escape is.
///
/// Accepted limitation: there is a window between this check and the read
/// or write a caller performs right after it, in which the path on disk
/// could in principle be swapped for a symlink. This is accepted for now
/// rather than closed (closing it properly needs directory-fd based,
/// `openat(O_NOFOLLOW)` style resolution, so the check and the use are the
/// same open file description, not two path lookups) because nothing in
/// this phase or the next runs a second, concurrent writer against the
/// same worktree while one tool call is in flight: that only starts once
/// the DAG executor's fan-out ships. Revisit this before fan-out lands.
///
/// # Errors
/// [`ToolError::OutsideWorktree`] if `relative` escapes the worktree
/// lexically or, once resolved, by a symlink; [`ToolError::Io`] if the
/// target does not exist or cannot be resolved.
pub fn resolve_within(worktree: &Path, relative: &str) -> Result<PathBuf, ToolError> {
    let candidate = worktree.join(relative);
    if !is_within(worktree, &candidate) {
        return Err(ToolError::OutsideWorktree {
            path: relative.to_string(),
        });
    }
    let canonical_worktree =
        std::fs::canonicalize(worktree).map_err(|error| ToolError::Io(error.to_string()))?;
    let canonical_candidate =
        std::fs::canonicalize(&candidate).map_err(|error| ToolError::Io(error.to_string()))?;
    if canonical_candidate.starts_with(&canonical_worktree) {
        Ok(canonical_candidate)
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

    #[test]
    fn resolve_within_accepts_a_relative_path_inside_the_worktree() {
        let worktree = tempfile::tempdir().unwrap();
        std::fs::create_dir(worktree.path().join("src")).unwrap();
        std::fs::write(worktree.path().join("src/lib.rs"), "").unwrap();
        let resolved = resolve_within(worktree.path(), "src/lib.rs").unwrap();
        assert_eq!(
            resolved,
            worktree.path().canonicalize().unwrap().join("src/lib.rs")
        );
    }

    #[test]
    fn resolve_within_refuses_a_path_that_climbs_out_lexically() {
        let worktree = tempfile::tempdir().unwrap();
        let error = resolve_within(worktree.path(), "../secret.txt").unwrap_err();
        assert!(matches!(error, ToolError::OutsideWorktree { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_within_refuses_a_symlink_that_escapes_the_worktree() {
        let worktree = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.txt");
        std::fs::write(&secret, "do not read me").unwrap();
        std::os::unix::fs::symlink(&secret, worktree.path().join("innocent.txt")).unwrap();

        let error = resolve_within(worktree.path(), "innocent.txt").unwrap_err();

        assert!(
            matches!(error, ToolError::OutsideWorktree { .. }),
            "a symlink pointing outside the worktree must be refused, got {error:?}"
        );
    }
}

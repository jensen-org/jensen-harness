use std::path::Path;

use super::ToolError;
use crate::sandbox::is_within;

/// Lists paths under `worktree` matching `pattern`, relative to the
/// worktree, sorted. Any match the glob library resolves outside the
/// worktree (a symlink climbing out, for instance) is dropped rather than
/// returned, since this tool never reports a path it would not also let a
/// caller read.
///
/// # Errors
/// [`ToolError::Io`] if `pattern` is not a valid glob.
pub fn run(worktree: &Path, pattern: &str) -> Result<Vec<String>, ToolError> {
    let joined = worktree.join(pattern);
    let full_pattern = joined.to_string_lossy().into_owned();
    let mut matches: Vec<String> = ::glob::glob(&full_pattern)
        .map_err(|error| ToolError::Io(error.to_string()))?
        .filter_map(Result::ok)
        .filter(|path| is_within(worktree, path))
        .filter_map(|path| {
            path.strip_prefix(worktree)
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        })
        .collect();
    matches.sort();
    Ok(matches)
}
